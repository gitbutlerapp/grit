//! `grit add` — add file contents to the index.
//!
//! Stages files from the working tree into the index so they will be
//! included in the next commit.

use crate::commands::sparse_advice::emit_sparse_path_advice;
use crate::explicit_exit::SilentNonZeroExit;
use anyhow::{anyhow, bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::attributes::{parse_gitattributes_file_content, validate_rules_for_add};
use grit_lib::config::ConfigSet;
use grit_lib::crlf::{self, ConversionConfig, GitAttributes};
use grit_lib::diff::{diff_index_to_worktree, DiffStatus};
use grit_lib::error::Error;
use grit_lib::ignore::{path_in_sparse_checkout as path_in_sparse_checkout_lines, IgnoreMatcher};
use grit_lib::index::{entry_from_metadata, normalize_mode, Index, IndexEntry, MODE_TREE};
#[allow(unused_imports)]
use grit_lib::objects::ObjectId;
use grit_lib::objects::{parse_commit, parse_tree, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::sparse_checkout::{
    effective_cone_mode_for_sparse_file, parse_sparse_checkout_file,
    path_in_sparse_checkout_patterns,
};
use grit_lib::state::resolve_head;
use grit_lib::unicode_normalization::{precompose_utf8_path, precompose_utf8_segment};
use grit_lib::wildmatch::wildmatch;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::commands::apply;
use crate::commands::commit::launch_commit_editor;
use crate::commands::diff::unstaged_patch_for_add_edit;

/// Error marker when adding a **new** path that lies outside the sparse-checkout definition.
#[derive(Debug, Clone)]
struct AddOutsideSparse {
    path: String,
}

impl std::fmt::Display for AddOutsideSparse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "outside sparse-checkout: {}", self.path)
    }
}

impl std::error::Error for AddOutsideSparse {}

/// Error when a nested repository has no checked-out commit (empty `HEAD` / unborn branch).
#[derive(Debug)]
struct EmbeddedRepoNoCommitError(String);

impl std::fmt::Display for EmbeddedRepoNoCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let base = self.0.trim_end_matches('/');
        write!(f, "'{base}/' does not have a commit checked out")
    }
}

impl std::error::Error for EmbeddedRepoNoCommitError {}

/// Read `extensions.objectformat` from a repository `config` (default `sha1`).
fn read_object_format_from_git_dir(git_dir: &Path) -> String {
    let config_path = git_dir.join("config");
    let Ok(content) = fs::read_to_string(&config_path) else {
        return "sha1".to_owned();
    };
    let mut in_extensions = false;
    let mut object_format: Option<String> = None;
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
        if k.trim().eq_ignore_ascii_case("objectformat") {
            object_format = Some(v.trim().to_lowercase());
        }
    }
    object_format.unwrap_or_else(|| "sha1".to_owned())
}

thread_local! {
    static DRY_RUN_STDOUT_LINES: RefCell<Vec<String>> = RefCell::new(Vec::new());
    static DRY_RUN_CAPTURE_MULTISPEC: Cell<bool> = Cell::new(false);
    static EMBEDDED_REPO_FULL_HINT_EMITTED: Cell<bool> = Cell::new(false);
}

fn dry_run_stdout_begin_multispec_capture() {
    DRY_RUN_CAPTURE_MULTISPEC.set(true);
    DRY_RUN_STDOUT_LINES.with(|v| v.borrow_mut().clear());
}

fn dry_run_stdout_push_line(line: String) {
    if DRY_RUN_CAPTURE_MULTISPEC.get() {
        DRY_RUN_STDOUT_LINES.with(|v| v.borrow_mut().push(line));
    } else {
        println!("{line}");
    }
}

fn dry_run_stdout_finish_multispec_capture() {
    if !DRY_RUN_CAPTURE_MULTISPEC.get() {
        return;
    }
    DRY_RUN_CAPTURE_MULTISPEC.set(false);
    DRY_RUN_STDOUT_LINES.with(|v| {
        let mut lines = std::mem::take(&mut *v.borrow_mut());
        lines.sort();
        for line in lines {
            println!("{line}");
        }
    });
}

fn dry_run_stdout_abort_multispec_capture() {
    if !DRY_RUN_CAPTURE_MULTISPEC.get() {
        return;
    }
    DRY_RUN_CAPTURE_MULTISPEC.set(false);
    DRY_RUN_STDOUT_LINES.with(|v| {
        v.borrow_mut().clear();
    });
}

fn finish_dry_stdout_before_exit() {
    dry_run_stdout_finish_multispec_capture();
}

struct DryRunMultispecStdoutGuard(bool);

impl DryRunMultispecStdoutGuard {
    fn maybe_begin(dry_run: bool, pathspec_count: usize) -> Self {
        if dry_run && pathspec_count > 1 {
            dry_run_stdout_begin_multispec_capture();
            Self(true)
        } else {
            Self(false)
        }
    }
}

impl Drop for DryRunMultispecStdoutGuard {
    fn drop(&mut self) {
        if self.0 {
            dry_run_stdout_finish_multispec_capture();
        }
    }
}

fn error_is_chmod_on_non_regular(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("cannot chmod")
}

pub(crate) fn resolved_env_index_path(repo: &Repository) -> PathBuf {
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

/// `git add -e`: write unstaged diff to `ADD_EDIT.patch`, run the editor, apply with `apply --recount --cached`.
fn run_add_edit(repo: &Repository, pathspecs: &[String]) -> Result<()> {
    let patch_path = repo.git_dir.join("ADD_EDIT.patch");
    let initial = unstaged_patch_for_add_edit(repo, pathspecs)?;
    if initial.trim().is_empty() {
        bail!("No changes.");
    }
    fs::write(&patch_path, initial).with_context(|| format!("writing {}", patch_path.display()))?;
    launch_commit_editor(repo, &patch_path).context("editing patch failed")?;
    let edited = fs::read_to_string(&patch_path)
        .with_context(|| format!("reading {}", patch_path.display()))?;
    if edited.trim().is_empty() {
        let _ = fs::remove_file(&patch_path);
        bail!("empty patch. aborted");
    }
    let apply_args = apply::Args {
        cached: true,
        recount: true,
        strip: 1,
        patches: vec![patch_path.clone()],
        ..Default::default()
    };
    apply::run(apply_args)
        .with_context(|| format!("could not apply '{}'", patch_path.display()))?;
    let _ = fs::remove_file(&patch_path);
    Ok(())
}

/// Resolve the number of context lines for `git add -p`: the `-U`/`--unified` flag wins (when
/// `>= 0`); otherwise `diff.context` (rejecting negatives like Git's `add-patch.c`); default 3.
pub(crate) fn resolve_patch_context(unified: Option<i32>, config: &ConfigSet) -> Result<usize> {
    if let Some(n) = unified {
        if n >= 0 {
            return Ok(n as usize);
        }
    }
    if let Some(v) = config.get("diff.context") {
        if let Ok(n) = v.trim().parse::<i32>() {
            if n < 0 {
                bail!("{} cannot be negative", "diff.context");
            }
            return Ok(n as usize);
        }
    }
    Ok(3)
}

/// Resolve the inter-hunk context for `git add -p`: `--inter-hunk-context` flag wins (when
/// `>= 0`); otherwise `diff.interhunkcontext`; default 0.
pub(crate) fn resolve_patch_interhunk(inter: Option<i32>, config: &ConfigSet) -> Result<usize> {
    if let Some(n) = inter {
        if n >= 0 {
            return Ok(n as usize);
        }
    }
    if let Some(v) = config.get("diff.interhunkcontext") {
        if let Ok(n) = v.trim().parse::<i32>() {
            if n < 0 {
                bail!("{} cannot be negative", "diff.interhunkcontext");
            }
            return Ok(n as usize);
        }
    }
    Ok(0)
}

/// Validate the interactive context options (`-U`/`--unified`, `--inter-hunk-context`) shared by
/// the patch-capable commands (`add`/`checkout`/`restore`/`reset`/`commit`/`stash`). Mirrors Git's
/// `parse_opt_unified`/`add-interactive` checks: a value below the `-1` "unset" sentinel is
/// rejected as negative, and outside `-p`/`-i` the options are an error.
pub(crate) fn validate_patch_context_options(
    unified: Option<i32>,
    inter_hunk_context: Option<i32>,
    interactive: bool,
) -> Result<()> {
    if let Some(n) = unified {
        if n < -1 {
            bail!("'{}' cannot be negative", "--unified");
        }
    }
    if let Some(n) = inter_hunk_context {
        if n < -1 {
            bail!("'{}' cannot be negative", "--inter-hunk-context");
        }
    }
    if !interactive {
        if unified.is_some() {
            bail!(
                "the option '{}' requires '{}'",
                "--unified",
                "--interactive/--patch"
            );
        }
        if inter_hunk_context.is_some() {
            bail!(
                "the option '{}' requires '{}'",
                "--inter-hunk-context",
                "--interactive/--patch"
            );
        }
    }
    Ok(())
}

/// Arguments for `grit add`.
#[derive(Debug, ClapArgs)]
#[command(about = "Add file contents to the index")]
pub struct Args {
    /// Files to add. Use '.' to add everything.
    #[arg(value_name = "PATHSPEC", num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
    pub pathspec: Vec<String>,

    /// Update tracked files (don't add new files).
    #[arg(short = 'u', long = "update")]
    pub update: bool,

    /// Add, modify, and remove index entries to match the working tree.
    #[arg(short = 'A', long = "all", alias = "no-ignore-removal")]
    pub all: bool,

    /// Only update already-tracked files, don't add new ones.
    #[arg(long = "no-all", alias = "ignore-removal")]
    pub no_all: bool,

    /// Record only the intent to add a path (placeholder entry).
    #[arg(short = 'N', long = "intent-to-add")]
    pub intent_to_add: bool,

    /// Dry run — show what would be added.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Be verbose.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Allow adding otherwise ignored files.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Interactive patch mode.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Interactive add mode.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Edit the diff vs. the index before staging.
    #[arg(short = 'e', long = "edit")]
    pub edit: bool,

    /// Override the file mode for the added files (+x or -x).
    #[arg(long = "chmod")]
    pub chmod: Option<String>,

    /// Renormalize tracked files (apply clean/smudge filters).
    #[arg(long = "renormalize")]
    pub renormalize: bool,

    /// Refresh stat info in the index without changing content.
    #[arg(long = "refresh")]
    pub refresh: bool,

    /// Continue adding files when some cannot be added.
    #[arg(long = "ignore-errors")]
    pub ignore_errors: bool,

    /// Force failing paths to abort even when `add.ignore-errors` is set in config.
    #[arg(long = "no-ignore-errors")]
    pub no_ignore_errors: bool,

    /// Allow updating index entries outside the sparse-checkout definition (and skip-worktree).
    #[arg(long = "sparse")]
    pub sparse: bool,

    /// Suppress warning for non-existent pathspecs (with --refresh).
    #[arg(long = "ignore-missing")]
    pub ignore_missing: bool,

    /// Suppress warning for adding an embedded repository.
    #[arg(long = "no-warn-embedded-repo")]
    pub no_warn_embedded_repo: bool,

    /// Read pathspecs from a file (one per line, or NUL-separated with --pathspec-file-nul).
    #[arg(long = "pathspec-from-file", value_name = "FILE")]
    pub pathspec_from_file: Option<PathBuf>,

    /// NUL-terminated pathspec input (requires --pathspec-from-file).
    #[arg(long = "pathspec-file-nul")]
    pub pathspec_file_nul: bool,

    /// Number of context lines in interactive (`-p`/`-i`) hunks. Mirrors Git's `-U`/`--unified`
    /// sentinel of `-1` (unset) so `add --unified` outside patch mode errors like Git.
    #[arg(
        short = 'U',
        long = "unified",
        value_name = "N",
        allow_hyphen_values = true
    )]
    pub unified: Option<i32>,

    /// Number of context lines between adjacent interactive hunks (`-p`/`-i`).
    #[arg(
        long = "inter-hunk-context",
        value_name = "N",
        allow_hyphen_values = true
    )]
    pub inter_hunk_context: Option<i32>,

    /// Disable auto-advancing to the next hunk after a decision in interactive patch mode.
    #[arg(long = "no-auto-advance")]
    pub no_auto_advance: bool,

    /// Re-enable auto-advancing (Git default); accepted for parity with `--no-auto-advance`.
    #[arg(long = "auto-advance", overrides_with = "no_auto_advance")]
    pub auto_advance: bool,
}

/// Flags for [`stage_file`] shared by `git add` and `git commit <paths>`.
pub(crate) struct StageFileContext<'a> {
    pub dry_run: bool,
    pub verbose: bool,
    pub intent_to_add: bool,
    pub chmod: Option<&'a str>,
}

impl StageFileContext<'_> {
    /// Staging as performed by `git commit` pathspecs (no dry-run, no chmod, no intent-to-add).
    pub fn for_commit() -> Self {
        Self {
            dry_run: false,
            verbose: false,
            intent_to_add: false,
            chmod: None,
        }
    }
}

impl<'a> From<&'a Args> for StageFileContext<'a> {
    fn from(a: &'a Args) -> Self {
        Self {
            dry_run: a.dry_run,
            verbose: a.verbose,
            intent_to_add: a.intent_to_add,
            chmod: a.chmod.as_deref(),
        }
    }
}

/// Run the `add` command.
pub fn run(mut args: Args) -> Result<()> {
    if args.pathspec.iter().any(|arg| arg == "--sparse") {
        args.sparse = true;
        args.pathspec.retain(|arg| arg != "--sparse");
    }

    if args.pathspec_file_nul && args.pathspec_from_file.is_none() {
        bail!("the option '--pathspec-file-nul' requires '--pathspec-from-file'");
    }
    if let Some(ref file) = args.pathspec_from_file {
        if !args.pathspec.is_empty() {
            bail!("'--pathspec-from-file' and pathspec arguments cannot be used together");
        }
        if args.interactive || args.patch {
            bail!(
                "options '--pathspec-from-file' and '--interactive/--patch' cannot be used together"
            );
        }
        if args.edit {
            bail!("options '--pathspec-from-file' and '--edit' cannot be used together");
        }
        let path = file.as_os_str();
        let data = if path == "-" {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .context("reading pathspecs from stdin")?;
            buf
        } else {
            fs::read(file)
                .with_context(|| format!("cannot read pathspec file '{}'", file.display()))?
        };
        args.pathspec =
            grit_lib::pathspec::parse_pathspecs_from_source(&data, args.pathspec_file_nul)?;
    }

    // Validate the interactive context options (`-U`/`--unified`, `--inter-hunk-context`,
    // `--no-auto-advance`) exactly like Git's `cmd_add` (builtin/add.c): negative values below the
    // `-1` sentinel are rejected, and the options only make sense with `-p`/`-i`.
    if let Some(n) = args.unified {
        if n < -1 {
            bail!("'{}' cannot be negative", "--unified");
        }
    }
    if let Some(n) = args.inter_hunk_context {
        if n < -1 {
            bail!("'{}' cannot be negative", "--inter-hunk-context");
        }
    }
    let interactive_mode = args.interactive || args.patch;
    if !interactive_mode {
        if args.unified.is_some() {
            bail!(
                "the option '{}' requires '{}'",
                "--unified",
                "--interactive/--patch"
            );
        }
        if args.inter_hunk_context.is_some() {
            bail!(
                "the option '{}' requires '{}'",
                "--inter-hunk-context",
                "--interactive/--patch"
            );
        }
        if args.no_auto_advance {
            bail!(
                "the option '{}' requires '{}'",
                "--no-auto-advance",
                "--interactive/--patch"
            );
        }
    }

    // --dry-run is incompatible with interactive modes
    if args.dry_run && (args.interactive || args.patch || args.edit) {
        bail!("options '--dry-run' and '--interactive'/'--patch'/'--edit' cannot be used together");
    }

    if args.edit {
        let repo = Repository::discover(None).context("not a git repository")?;
        repo.work_tree
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
        run_add_edit(&repo, &args.pathspec)?;
        return Ok(());
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;

    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let core_filemode = config
        .get_bool("core.filemode")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let precompose_unicode =
        grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir));

    let index_path = resolved_env_index_path(&repo);
    // Git holds the index lock at the start of `cmd_add` (LOCK_REPORT_ON_ERROR), even with
    // --dry-run, so a pre-existing `index.lock` is fatal. grit only checks the lock when it
    // writes the index, which dry-run skips — replicate the early check here so
    // `git submodule add` relays the lock error (t7400 "relays add --dry-run stderr").
    {
        let lock_path = index_path.with_extension("lock");
        if lock_path.exists() {
            let mut msg = format!("Unable to create '{}': File exists.", lock_path.display());
            if let Some(pid_msg) = lockfile_pid_diagnostic(&index_path) {
                msg.push_str("\n\n");
                msg.push_str(&pid_msg);
            } else {
                msg.push_str("\n\n");
                msg.push_str(
                    "Another git process seems to be running in this repository, or the lock file may be stale",
                );
            }
            bail!("fatal: {msg}");
        }
    }
    let idx_exists = index_path.exists();
    // Resolve the on-disk index format exactly once, matching Git's
    // `get_index_format_default` (otherwise `GIT_INDEX_VERSION`/`index.version`
    // warnings would be emitted twice). For a fresh index, `raw_index` is only an
    // empty sparse-index baseline, so it reuses the already-resolved version
    // instead of re-reading the environment.
    let mut index = if idx_exists {
        repo.load_index_at(&index_path)?
    } else {
        Index::new_from_config(&config)
    };
    let raw_index = if idx_exists {
        Index::load(&index_path).unwrap_or_else(|_| Index::empty(index.version))
    } else {
        Index::empty(index.version)
    };

    let odb = &repo.odb;

    // Resolve the current working directory relative to the worktree
    let cwd = std::env::current_dir()?;
    let prefix = crate::pathspec::pathdiff(&cwd, work_tree);
    die_if_in_unpopulated_submodule(&index, prefix.as_deref());

    let sparse_state = AddSparseState::load(&repo, &config);

    // Validate empty string pathspecs
    for ps in &args.pathspec {
        if ps.is_empty() {
            bail!("invalid path ''");
        }
    }

    let conv = ConversionConfig::from_config(&config);
    let attrs = crlf::load_gitattributes(work_tree);
    let add_cfg = AddConfig {
        core_filemode,
        precompose_unicode,
        ignore_errors: (args.ignore_errors
            || config
                .get_bool("add.ignore-errors")
                .and_then(|r| r.ok())
                .unwrap_or(false))
            && !args.no_ignore_errors,
        conv,
        attrs,
        config: config.clone(),
        sparse: sparse_state.clone(),
        include_sparse: args.sparse,
        large_blobs: (!args.dry_run)
            .then(|| LargeBlobBatch::from_config(&config).map(|batch| Rc::new(RefCell::new(batch))))
            .flatten(),
    };

    // Exclude-only pathspec lists (`:(exclude)`, `:!`, …) are handled like plain `git add` for
    // index updates; `add -p` would use `run_add_patch`, which does not implement exclude
    // semantics (t6132 `add -p with all negative`).
    if args.patch && !pathspecs_are_all_exclude(&args.pathspec) {
        let patch_opts = super::add_patch::PatchOptions {
            context: resolve_patch_context(args.unified, &config)?,
            inter_hunk_context: resolve_patch_interhunk(args.inter_hunk_context, &config)?,
            auto_advance: !args.no_auto_advance,
        };
        return super::add_patch::run_add_patch(&repo, &args.pathspec, &add_cfg, &patch_opts);
    }
    if args.interactive {
        maybe_emit_interactive_add_sparse_index_trace(&repo, &raw_index, &index, work_tree)?;
        return super::add_interactive::run_add_i(
            &repo,
            index,
            work_tree,
            &config,
            &add_cfg,
            &args.pathspec,
        );
    }

    let _dry_stdout_guard =
        DryRunMultispecStdoutGuard::maybe_begin(args.dry_run, args.pathspec.len());

    // "git add" with no pathspecs and no -A/-u/--refresh: give advice and do nothing.
    // Per Git (`require_pathspec && pathspec.nr == 0`), `--chmod` does NOT exempt this — a bare
    // `git add --chmod=+x` prints "Nothing specified" and leaves the index untouched (Git only
    // runs `chmod_pathspec` when `pathspec.nr` is nonzero), so we must not flip any entries here.
    if args.pathspec.is_empty() && !args.all && !args.update && !args.refresh {
        eprintln!("Nothing specified, nothing added.");
        eprintln!("hint: Maybe you wanted to say 'git add .'?");
        eprintln!(
            "hint: Disable this message with \"git config set advice.addEmptyPathspec false\""
        );
        return Ok(());
    }

    // --refresh mode
    if args.refresh {
        return run_refresh(
            &repo,
            &mut index,
            work_tree,
            prefix.as_deref(),
            &args,
            &sparse_state,
            &config,
        );
    }

    // Build ignore matcher if needed (not needed with --force)
    let mut ignore_matcher = if !args.force {
        Some(IgnoreMatcher::from_repository(&repo)?)
    } else {
        None
    };

    // --renormalize: re-apply clean conversion to tracked files
    if args.renormalize {
        return run_renormalize(
            &repo,
            odb,
            &mut index,
            work_tree,
            prefix.as_deref(),
            &args,
            &add_cfg,
        );
    }

    // NOTE: `--chmod` WITH pathspecs flows through the normal add path below. Git stages the
    // matched paths first (`add_files_to_cache`/`add_files`), then applies the chmod flip
    // (`chmod_pathspec`); our `stage_file` applies the chmod inline while staging, which is
    // equivalent. The standalone `chmod_index_entries` helper is only used for the
    // no-pathspec case above (Git's `chmod_arg && pathspec.nr` guard).

    let mut exit_for_sparse_advice = false;
    let mut sparse_advice_paths: Vec<String> = Vec::new();

    let resolved_specs = resolved_pathspecs_for_add(&args.pathspec, work_tree, prefix.as_deref())?;
    let is_root_pathspec = args.pathspec.iter().any(|p| p == ":/");
    let mut silent_exit_after_write: Option<i32> = None;
    let mut deferred_chmod: Option<anyhow::Error> = None;
    if args.all || args.pathspec.iter().any(|p| p == ".") || is_root_pathspec {
        if !args.sparse {
            for spec in &args.pathspec {
                if spec == "." || spec == ":/" {
                    let resolved =
                        crate::pathspec::resolve_pathspec(spec, work_tree, prefix.as_deref());
                    if pathspec_only_matches_sparse_blocked(
                        spec,
                        &resolved,
                        &index,
                        work_tree,
                        &sparse_state,
                        precompose_unicode,
                        &repo,
                        &mut ignore_matcher,
                    ) {
                        sparse_advice_paths.push(spec.clone());
                    }
                }
            }
        }
        let effective_prefix = if is_root_pathspec {
            None
        } else {
            prefix.as_deref()
        };
        if args.all
            && !args.pathspec.is_empty()
            && !is_root_pathspec
            && !args.pathspec.iter().any(|p| p == ".")
            && !pathspecs_are_all_exclude(&resolved_specs)
        {
            add_all_for_pathspecs(
                odb,
                &mut index,
                work_tree,
                prefix.as_deref(),
                &args.pathspec,
                &args,
                &repo,
                &mut ignore_matcher,
                &add_cfg,
                &mut sparse_advice_paths,
            )?;
        } else {
            let (need_exit1, chmod_e) = add_all(
                odb,
                &mut index,
                work_tree,
                effective_prefix,
                &resolved_specs,
                &args,
                &repo,
                &mut ignore_matcher,
                &add_cfg,
                &mut sparse_advice_paths,
            )?;
            if need_exit1 {
                silent_exit_after_write = Some(1);
            }
            deferred_chmod = chmod_e;
        }
    } else if args.update {
        update_tracked(
            odb,
            &mut index,
            work_tree,
            prefix.as_deref(),
            &args,
            &repo,
            &add_cfg,
            &mut sparse_advice_paths,
        )?;
    } else if pathspecs_are_all_exclude(&resolved_specs) {
        let _ = add_with_pathspec_list(
            odb,
            &mut index,
            work_tree,
            &resolved_specs,
            &args,
            &repo,
            &mut ignore_matcher,
            &add_cfg,
        )?;
    } else {
        if !args.sparse {
            for spec in &args.pathspec {
                let resolved =
                    crate::pathspec::resolve_pathspec(spec, work_tree, prefix.as_deref());
                if pathspec_only_matches_sparse_blocked(
                    spec,
                    &resolved,
                    &index,
                    work_tree,
                    &sparse_state,
                    precompose_unicode,
                    &repo,
                    &mut ignore_matcher,
                ) {
                    sparse_advice_paths.push(spec.clone());
                }
            }
        }
        let mut had_errors = false;
        let mut had_ignored = false;
        let mut chmod_deferred: Option<anyhow::Error> = None;
        if pathspecs_need_match_walk(&args.pathspec) {
            let matched = add_with_pathspec_list(
                odb,
                &mut index,
                work_tree,
                &resolved_specs,
                &args,
                &repo,
                &mut ignore_matcher,
                &add_cfg,
            )?;
            if !matched && !(args.dry_run && args.ignore_missing) {
                let pathspec = args.pathspec.first().map_or("", String::as_str);
                if args.dry_run {
                    println!("fatal: pathspec '{pathspec}' did not match any files");
                    return Err(anyhow::Error::new(SilentNonZeroExit { code: 128 }));
                }
                bail!("pathspec '{}' did not match any files", pathspec);
            }
        } else {
            for (_pathspec, resolved) in args.pathspec.iter().zip(resolved_specs.iter()) {
                // Expand glob patterns (e.g. "file?.t", "*.c") against the working tree.
                let expanded =
                    expand_glob_pathspec(resolved, work_tree, add_cfg.precompose_unicode);
                for resolved in &expanded {
                    match add_path(
                        odb,
                        &mut index,
                        work_tree,
                        resolved,
                        &args,
                        &repo,
                        &mut ignore_matcher,
                        &add_cfg,
                    ) {
                        Ok(()) => {}
                        Err(AddPathError::Ignored(msg)) => {
                            eprintln!("{msg}");
                            had_errors = true;
                            if !(args.dry_run && args.ignore_missing) {
                                had_ignored = true;
                            }
                        }
                        Err(AddPathError::DryRunFatalPathspec { message }) => {
                            println!("{message}");
                            return Err(anyhow::Error::new(SilentNonZeroExit { code: 128 }));
                        }
                        Err(AddPathError::EmbeddedNoCommit { rel_path }) => {
                            eprintln!("error: '{rel_path}' does not have a commit checked out");
                            eprintln!("error: unable to index file '{rel_path}'");
                            eprintln!("fatal: adding files failed");
                            std::process::exit(128);
                        }
                        Err(AddPathError::IoError(e)) => {
                            if add_cfg.ignore_errors {
                                eprintln!("warning: {e}");
                                had_errors = true;
                            } else if args.chmod.is_some() && error_is_chmod_on_non_regular(&e) {
                                chmod_deferred = chmod_deferred.or(Some(e));
                                had_errors = true;
                            } else {
                                if is_unwritable_odb_error(&e) {
                                    eprintln!(
                                        "error: insufficient permission for adding an object to repository database .git/objects"
                                    );
                                    eprintln!(
                                        "error: {}: failed to insert into database",
                                        resolved
                                    );
                                    eprintln!("error: unable to index file '{}'", resolved);
                                    eprintln!("fatal: updating files failed");
                                    std::process::exit(1);
                                }
                                return Err(e);
                            }
                        }
                        Err(AddPathError::OutsideSparse(path)) => {
                            sparse_advice_paths.push(path);
                            had_errors = true;
                        }
                        Err(AddPathError::Other(e)) => {
                            if add_cfg.ignore_errors {
                                eprintln!("warning: {e}");
                                had_errors = true;
                            } else if args.chmod.is_some() && error_is_chmod_on_non_regular(&e) {
                                chmod_deferred = chmod_deferred.or(Some(e));
                                had_errors = true;
                            } else {
                                return Err(e);
                            }
                        }
                    }
                } // end expanded loop
            }
        }

        deferred_chmod = chmod_deferred;

        if had_ignored {
            if !args.dry_run {
                write_index_or_lock_err(&repo, &mut index, &index_path)?;
            }
            bail!("some ignored files could not be added");
        }
        if had_errors {
            sparse_advice_paths.sort();
            sparse_advice_paths.dedup();
            if !sparse_advice_paths.is_empty() {
                emit_sparse_path_advice(
                    &mut std::io::stderr(),
                    &add_cfg.config,
                    &sparse_advice_paths,
                )?;
                if !args.dry_run {
                    write_index_or_lock_err(&repo, &mut index, &index_path)?;
                }
                std::process::exit(1);
            }
            if !args.dry_run {
                write_index_or_lock_err(&repo, &mut index, &index_path)?;
            }
            if add_cfg.ignore_errors {
                finish_dry_stdout_before_exit();
                return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
            }
            if args.dry_run && args.ignore_missing {
                finish_dry_stdout_before_exit();
                return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
            }
            if deferred_chmod.is_none() {
                bail!("adding files failed");
            }
        }
    }

    sparse_advice_paths.sort();
    sparse_advice_paths.dedup();
    if !sparse_advice_paths.is_empty() {
        emit_sparse_path_advice(
            &mut std::io::stderr(),
            &add_cfg.config,
            &sparse_advice_paths,
        )?;
        exit_for_sparse_advice = true;
    }

    if !args.dry_run {
        flush_large_blob_batch(&add_cfg, &repo)?;
        write_index_or_lock_err(&repo, &mut index, &index_path)?;
    }

    if exit_for_sparse_advice {
        std::process::exit(1);
    }

    if let Some(e) = deferred_chmod {
        return Err(e);
    }

    if let Some(code) = silent_exit_after_write {
        return Err(anyhow::Error::new(SilentNonZeroExit { code }));
    }

    Ok(())
}

/// Sparse-checkout state for `git add` (matches `path_in_sparse_checkout` / `core.sparseCheckout`).
#[derive(Clone)]
pub(crate) struct AddSparseState {
    sparse_enabled: bool,
    /// Whether cone-style pattern matching applies to `patterns` (Git `effective_cone_mode_for_sparse_file`).
    effective_cone: bool,
    patterns: Vec<String>,
}

impl AddSparseState {
    pub(crate) fn load(repo: &Repository, config: &ConfigSet) -> Self {
        let sparse_enabled = config
            .get("core.sparseCheckout")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let cone_cfg = config
            .get("core.sparseCheckoutCone")
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(true);
        let patterns: Vec<String> = if sparse_enabled {
            let sc_path = repo.git_dir.join("info").join("sparse-checkout");
            match fs::read_to_string(&sc_path) {
                Ok(s) => parse_sparse_checkout_file(&s),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let effective_cone = effective_cone_mode_for_sparse_file(cone_cfg, &patterns);
        Self {
            sparse_enabled,
            effective_cone,
            patterns,
        }
    }

    /// Whether `path` is inside the sparse-checkout definition (always true when sparse is off).
    ///
    /// Non-cone mode uses Git's `path_in_sparse_checkout` (parent walk + last-match), not sequential
    /// `NonConePatterns` toggles — required for `t3705` / `git sparse-checkout set a` with file `a`.
    fn path_in_sparse_definition(&self, path: &str) -> bool {
        if !self.sparse_enabled || self.patterns.is_empty() {
            return true;
        }
        if self.effective_cone {
            path_in_sparse_checkout_patterns(path, &self.patterns, true)
        } else {
            path_in_sparse_checkout_lines(path, &self.patterns, None)
        }
    }

    /// When `--sparse` is not set, refuse to update this path (skip-worktree or outside sparse cone).
    fn add_update_blocked(
        &self,
        include_sparse: bool,
        index_entry: Option<&IndexEntry>,
        path: &str,
    ) -> bool {
        if include_sparse {
            return false;
        }
        if let Some(e) = index_entry {
            if e.skip_worktree() {
                return true;
            }
        }
        if self.sparse_enabled && !self.path_in_sparse_definition(path) {
            return true;
        }
        false
    }
}

fn maybe_emit_interactive_add_sparse_index_trace(
    repo: &Repository,
    raw_index: &Index,
    index: &Index,
    work_tree: &Path,
) -> Result<()> {
    let entries = diff_index_to_worktree(&repo.odb, index, work_tree, false, false)?;
    if entries.iter().any(|entry| {
        entry.status != DiffStatus::Unmerged
            && path_under_sparse_index_dir(raw_index, &entry.path().to_string_lossy())
    }) {
        emit_index_trace_region("ensure_full_index");
    }
    Ok(())
}

fn path_under_sparse_index_dir(index: &Index, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    index
        .entries
        .iter()
        .filter(|entry| entry.stage() == 0 && entry.mode == MODE_TREE)
        .filter_map(|entry| std::str::from_utf8(&entry.path).ok())
        .map(|prefix| prefix.trim_end_matches('/'))
        .any(|prefix| {
            let prefix_slash = format!("{prefix}/");
            path == prefix || path.starts_with(&prefix_slash)
        })
}

fn emit_index_trace_region(label: &str) {
    if let Ok(trace2_event) = std::env::var("GIT_TRACE2_EVENT") {
        if !trace2_event.trim().is_empty() {
            let _ = crate::trace2_region_json(&trace2_event, "index", label);
        }
    }
}

/// Match a user pathspec against an index path (Git `ce_path_match` semantics for `.` and globs).
fn pathspec_matches_index_path(spec: &str, resolved_under_cwd: &str, index_path: &str) -> bool {
    if spec == "." {
        // `.` at the repo root matches the whole tree. `resolve_pathspec` keeps
        // it as the literal "." (rather than ""), so treat both "" and "." as
        // "match everything"; a non-trivial prefix matches that subtree.
        if resolved_under_cwd.is_empty() || resolved_under_cwd == "." {
            return true;
        }
        return index_path == resolved_under_cwd
            || index_path.starts_with(&format!("{resolved_under_cwd}/"));
    }
    grit_lib::pathspec::pathspec_matches(spec, index_path)
}

/// `true` when some stage-0 index path matches the pathspec and is allowed to update without `--sparse`.
fn pathspec_has_dense_index_match(
    spec: &str,
    resolved: &str,
    index: &Index,
    sparse: &AddSparseState,
) -> bool {
    for ie in &index.entries {
        if ie.stage() != 0 {
            continue;
        }
        let p = String::from_utf8_lossy(&ie.path);
        if !pathspec_matches_index_path(spec, resolved, p.as_ref()) {
            continue;
        }
        if !sparse.add_update_blocked(false, Some(ie), p.as_ref()) {
            return true;
        }
    }
    false
}

/// True if `spec` matches at least one path we could update without `--sparse` (index or work tree).
#[allow(clippy::too_many_arguments)]
fn pathspec_has_unblocked_target(
    spec: &str,
    resolved: &str,
    index: &Index,
    work_tree: &Path,
    sparse: &AddSparseState,
    precompose_unicode: bool,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
) -> bool {
    if pathspec_has_dense_index_match(spec, resolved, index, sparse) {
        return true;
    }
    for rel in expand_glob_pathspec(resolved, work_tree, precompose_unicode) {
        // A directory pathspec (e.g. `.`, `:/`, `dir`) matches every work-tree
        // file beneath it. Git's pathspec advice mirrors `fill_directory`: an
        // untracked ignored file is excluded and does not count as an addable
        // target, so it must not suppress the sparse-path advice. Walk the
        // directory and look for any genuinely addable file. (t3705 `git add .`)
        let abs = if rel.is_empty() || rel == "." {
            work_tree.to_path_buf()
        } else {
            work_tree.join(&rel)
        };
        let is_dir = fs::symlink_metadata(&abs)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        if is_dir {
            if dir_has_addable_target(&abs, work_tree, index, sparse, repo, ignore_matcher) {
                return true;
            }
            continue;
        }

        if rel.is_empty() {
            continue;
        }
        let ie = index.get(rel.as_bytes(), 0);
        if sparse.add_update_blocked(false, ie, rel.as_str()) {
            continue;
        }
        if ie.is_none() && path_is_ignored(repo, index, ignore_matcher, rel.as_str(), false) {
            continue;
        }
        return true;
    }
    false
}

/// Whether `repo_rel_path` is ignored by `.gitignore`/excludes (best effort).
fn path_is_ignored(
    repo: &Repository,
    index: &Index,
    ignore_matcher: &mut Option<IgnoreMatcher>,
    repo_rel_path: &str,
    is_dir: bool,
) -> bool {
    if let Some(matcher) = ignore_matcher.as_mut() {
        if let Ok((ignored, _)) = matcher.check_path(repo, Some(index), repo_rel_path, is_dir) {
            return ignored;
        }
    }
    false
}

/// Recursively look for a work-tree file under `dir` that `git add` could stage
/// without `--sparse`: tracked-and-not-sparse-blocked, or untracked-and-not-ignored.
/// Mirrors Git's `fill_directory` walk used for the sparse-path advice decision.
fn dir_has_addable_target(
    dir: &Path,
    work_tree: &Path,
    index: &Index,
    sparse: &AddSparseState,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
) -> bool {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return false;
    };
    for ent in read_dir.flatten() {
        let path = ent.path();
        let rel = match path.strip_prefix(work_tree) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if rel == ".git" || rel.starts_with(".git/") || rel.ends_with("/.git") {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_dir() {
            // An ignored directory prunes the whole subtree (Git stops descending).
            if path_is_ignored(repo, index, ignore_matcher, &rel, true) {
                continue;
            }
            if dir_has_addable_target(&path, work_tree, index, sparse, repo, ignore_matcher) {
                return true;
            }
            continue;
        }
        let ie = index.get(rel.as_bytes(), 0);
        if sparse.add_update_blocked(false, ie, rel.as_str()) {
            continue;
        }
        if ie.is_none() && path_is_ignored(repo, index, ignore_matcher, &rel, false) {
            continue;
        }
        return true;
    }
    false
}

/// Pathspec matched only skip-worktree / outside-sparse index entries (Git `matches_skip_worktree`).
#[allow(clippy::too_many_arguments)]
fn pathspec_only_matches_sparse_blocked(
    spec: &str,
    resolved: &str,
    index: &Index,
    work_tree: &Path,
    sparse: &AddSparseState,
    precompose_unicode: bool,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
) -> bool {
    if pathspec_has_unblocked_target(
        spec,
        resolved,
        index,
        work_tree,
        sparse,
        precompose_unicode,
        repo,
        ignore_matcher,
    ) {
        return false;
    }
    index.entries.iter().any(|ie| {
        if ie.stage() != 0 {
            return false;
        }
        let p = String::from_utf8_lossy(&ie.path);
        if !pathspec_matches_index_path(spec, resolved, p.as_ref()) {
            return false;
        }
        sparse.add_update_blocked(false, Some(ie), p.as_ref())
    })
}

pub(crate) struct AddConfig {
    pub core_filemode: bool,
    pub precompose_unicode: bool,
    pub ignore_errors: bool,
    pub conv: ConversionConfig,
    pub attrs: GitAttributes,
    pub config: ConfigSet,
    pub sparse: AddSparseState,
    pub include_sparse: bool,
    pub large_blobs: Option<Rc<RefCell<LargeBlobBatch>>>,
}

/// Pending large blob objects that should be written as packfiles at the end of `git add`.
pub(crate) struct LargeBlobBatch {
    threshold: u64,
    pack_size_limit: Option<u64>,
    entries: Vec<(ObjectId, ObjectKind, Vec<u8>)>,
    seen: HashSet<ObjectId>,
}

impl LargeBlobBatch {
    fn from_config(config: &ConfigSet) -> Option<Self> {
        let threshold = config_i64(config, "core.bigFileThreshold")
            .or_else(|| config_i64(config, "core.bigfilethreshold"))?;
        if threshold < 0 {
            return None;
        }
        let pack_size_limit = config_i64(config, "pack.packSizeLimit")
            .or_else(|| config_i64(config, "pack.packsizelimit"))
            .and_then(|v| u64::try_from(v).ok())
            .filter(|&v| v > 0);
        Some(Self {
            threshold: threshold as u64,
            pack_size_limit,
            entries: Vec::new(),
            seen: HashSet::new(),
        })
    }

    fn write_blob_or_queue(&mut self, odb: &Odb, data: &[u8]) -> Result<ObjectId> {
        let oid = Odb::hash_object_data(ObjectKind::Blob, data);
        if data.len() as u64 <= self.threshold || odb.exists(&oid) {
            return odb
                .write(ObjectKind::Blob, data)
                .map_err(anyhow::Error::from);
        }
        if self.seen.insert(oid) {
            self.entries.push((oid, ObjectKind::Blob, data.to_vec()));
        }
        Ok(oid)
    }

    fn flush(&mut self, repo: &Repository) -> Result<()> {
        if self.entries.is_empty() {
            return Ok(());
        }
        let entries = std::mem::take(&mut self.entries);
        self.seen.clear();
        if let Some(limit) = self.pack_size_limit {
            let mut chunk = Vec::new();
            let mut chunk_bytes = 0u64;
            for entry in entries {
                let entry_bytes = entry.2.len() as u64;
                if !chunk.is_empty() && chunk_bytes.saturating_add(entry_bytes) > limit {
                    crate::commands::pack_objects::write_full_object_pack(repo, &chunk)?;
                    chunk.clear();
                    chunk_bytes = 0;
                }
                chunk_bytes = chunk_bytes.saturating_add(entry_bytes);
                chunk.push(entry);
            }
            if !chunk.is_empty() {
                crate::commands::pack_objects::write_full_object_pack(repo, &chunk)?;
            }
        } else {
            crate::commands::pack_objects::write_full_object_pack(repo, &entries)?;
        }
        Ok(())
    }
}

fn config_i64(config: &ConfigSet, key: &str) -> Option<i64> {
    config.get_i64(key).and_then(|r| r.ok())
}

fn write_staged_blob(
    odb: &Odb,
    _repo: &Repository,
    data: &[u8],
    add_cfg: &AddConfig,
) -> Result<ObjectId> {
    if let Some(batch) = &add_cfg.large_blobs {
        return batch.borrow_mut().write_blob_or_queue(odb, data);
    }
    odb.write(ObjectKind::Blob, data)
        .map_err(anyhow::Error::from)
}

fn flush_large_blob_batch(add_cfg: &AddConfig, repo: &Repository) -> Result<()> {
    if let Some(batch) = &add_cfg.large_blobs {
        batch.borrow_mut().flush(repo)?;
    }
    Ok(())
}

/// Stage pathspecs the same way `git commit <paths>` does (recursive dirs, CRLF clean, etc.).
pub(crate) fn stage_pathspecs_for_commit(
    repo: &Repository,
    work_tree: &Path,
    pathspecs: &[String],
    add_cfg: &AddConfig,
) -> Result<HashSet<Vec<u8>>> {
    let index_path = resolved_env_index_path(repo);
    let mut index = match repo.load_index_at(&index_path) {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e.into()),
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| work_tree.to_path_buf());
    let prefix = crate::pathspec::pathdiff(&cwd, work_tree);

    let mut ignore_matcher = Some(IgnoreMatcher::from_repository(repo)?);
    let odb = &repo.odb;
    let ctx = StageFileContext::for_commit();

    let mut matched_paths = HashSet::new();
    let head_paths = commit_head_tree_paths(repo)?;

    let is_known_to_index = |idx: &Index, path: &[u8]| -> bool {
        if path.is_empty() || path == b"." {
            return !idx.entries.is_empty() || !head_paths.is_empty();
        }
        idx.get(path, 0).is_some()
            || head_paths.contains(path)
            || idx.entries.iter().any(|entry| {
                entry.stage() == 0
                    && entry.path.starts_with(path)
                    && entry.path.get(path.len()) == Some(&b'/')
            })
            || head_paths
                .iter()
                .any(|known| known.starts_with(path) && known.get(path.len()) == Some(&b'/'))
    };

    let remove_path_and_descendants = |idx: &mut Index, path: &[u8]| -> Vec<Vec<u8>> {
        let removed: Vec<Vec<u8>> = idx
            .entries
            .iter()
            .filter(|entry| {
                entry.path == path
                    || (entry.path.starts_with(path) && entry.path.get(path.len()) == Some(&b'/'))
            })
            .map(|entry| entry.path.clone())
            .collect();
        for p in &removed {
            idx.remove(p);
        }
        removed
    };

    let reject_skip_worktree = |idx: &Index, path: &[u8]| -> Result<()> {
        if idx.get(path, 0).is_some_and(|e| e.skip_worktree()) {
            bail!("cannot update skip-worktree entry");
        }
        Ok(())
    };

    let resolved_specs: Vec<String> = pathspecs
        .iter()
        .map(|s| crate::pathspec::resolve_pathspec(s, work_tree, prefix.as_deref()))
        .collect();

    if pathspecs_are_all_exclude(pathspecs) {
        let mut paths: Vec<(String, PathBuf)> = Vec::new();
        walk_directory(
            work_tree,
            work_tree,
            &mut paths,
            repo,
            &mut ignore_matcher,
            false,
            add_cfg.precompose_unicode,
        )?;
        let worktree_paths: HashSet<&str> = paths.iter().map(|(r, _)| r.as_str()).collect();

        let to_remove: Vec<Vec<u8>> = index
            .entries
            .iter()
            .filter(|ie| {
                if ie.skip_worktree() {
                    return false;
                }
                let p = String::from_utf8_lossy(&ie.path);
                grit_lib::pathspec::matches_pathspec_list(p.as_ref(), &resolved_specs)
                    && !worktree_paths.contains(p.as_ref())
            })
            .map(|ie| ie.path.clone())
            .collect();
        for p in to_remove {
            index.remove(&p);
            matched_paths.insert(p);
        }

        for (rel, abs_path) in paths {
            if !grit_lib::pathspec::matches_pathspec_list(&rel, &resolved_specs) {
                continue;
            }
            reject_skip_worktree(&index, rel.as_bytes())?;
            stage_file(
                odb, &mut index, work_tree, &rel, &abs_path, repo, &ctx, add_cfg,
            )?;
            matched_paths.insert(rel.as_bytes().to_vec());
        }

        repo.write_index_at(&index_path, &mut index)?;
        return Ok(matched_paths);
    }

    for spec in pathspecs {
        let resolved = crate::pathspec::resolve_pathspec(spec, work_tree, prefix.as_deref());

        if !grit_lib::pathspec::has_glob_chars(&resolved) {
            reject_skip_worktree(&index, resolved.as_bytes())?;
            if !is_known_to_index(&index, resolved.as_bytes()) {
                bail!("pathspec '{spec}' did not match any file(s) known to git");
            }
            let abs_path = work_tree.join(&resolved);
            let meta = match fs::symlink_metadata(&abs_path) {
                Ok(m) => m,
                Err(_) => {
                    let removed = remove_path_and_descendants(&mut index, resolved.as_bytes());
                    if removed.is_empty() {
                        matched_paths.insert(resolved.as_bytes().to_vec());
                    } else {
                        matched_paths.extend(removed);
                    }
                    continue;
                }
            };

            let is_real_dir = !meta.file_type().is_symlink() && meta.file_type().is_dir();
            if is_real_dir {
                if is_nested_embedded_git_repo(&abs_path, repo) {
                    stage_gitlink(
                        odb, &mut index, repo, &resolved, &abs_path, false, false, false,
                    )?;
                    matched_paths.insert(resolved.as_bytes().to_vec());
                    continue;
                }
                let rels = collect_paths_for_stage_from_directory(
                    &abs_path,
                    work_tree,
                    repo,
                    &mut ignore_matcher,
                    false,
                    add_cfg.precompose_unicode,
                )?;
                for (rel, file_abs) in rels {
                    if index.get(rel.as_bytes(), 0).is_none()
                        && !head_paths.contains(rel.as_bytes())
                    {
                        continue;
                    }
                    reject_skip_worktree(&index, rel.as_bytes())?;
                    stage_file(
                        odb, &mut index, work_tree, &rel, &file_abs, repo, &ctx, add_cfg,
                    )?;
                    matched_paths.insert(rel.as_bytes().to_vec());
                }
                continue;
            }

            stage_file(
                odb, &mut index, work_tree, &resolved, &abs_path, repo, &ctx, add_cfg,
            )?;
            matched_paths.insert(resolved.as_bytes().to_vec());
            continue;
        }

        let (dir_prefix, pattern) = if let Some(slash_pos) = resolved.rfind('/') {
            (&resolved[..slash_pos], &resolved[slash_pos + 1..])
        } else {
            ("", resolved.as_str())
        };

        let search_dir = if dir_prefix.is_empty() {
            work_tree.to_path_buf()
        } else {
            work_tree.join(dir_prefix)
        };

        let mut spec_matched = false;
        let mut matched_rels: Vec<String> = Vec::new();
        if let Ok(entries) = fs::read_dir(&search_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let raw_name = file_name.to_string_lossy();
                let name_str = if add_cfg.precompose_unicode {
                    precompose_utf8_segment(raw_name.as_ref()).into_owned()
                } else {
                    raw_name.into_owned()
                };
                if name_str == ".git" {
                    continue;
                }
                if !wildmatch(pattern.as_bytes(), name_str.as_bytes(), 0) {
                    continue;
                }
                let rel = if dir_prefix.is_empty() {
                    name_str.clone()
                } else {
                    format!("{dir_prefix}/{name_str}")
                };
                matched_rels.push(rel);
            }
        }
        if pattern.contains('[') && fs::symlink_metadata(search_dir.join(pattern)).is_ok() {
            let rel = if dir_prefix.is_empty() {
                pattern.to_string()
            } else {
                format!("{dir_prefix}/{pattern}")
            };
            if !matched_rels.contains(&rel) {
                matched_rels.push(rel);
            }
        }

        for rel in matched_rels {
            if index.get(rel.as_bytes(), 0).is_none() && !head_paths.contains(rel.as_bytes()) {
                continue;
            }
            reject_skip_worktree(&index, rel.as_bytes())?;
            let abs_path = work_tree.join(&rel);
            if fs::symlink_metadata(&abs_path).is_ok() {
                stage_file(
                    odb, &mut index, work_tree, &rel, &abs_path, repo, &ctx, add_cfg,
                )?;
                spec_matched = true;
                matched_paths.insert(rel.as_bytes().to_vec());
            }
        }

        if !spec_matched {
            bail!("pathspec '{spec}' did not match any file(s) known to git");
        }
    }

    repo.write_index_at(&index_path, &mut index)?;
    Ok(matched_paths)
}

fn commit_head_tree_paths(repo: &Repository) -> Result<HashSet<Vec<u8>>> {
    let head = resolve_head(&repo.git_dir)?;
    let Some(head_oid) = head.oid() else {
        return Ok(HashSet::new());
    };
    let obj = repo.odb.read(head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let mut paths = HashSet::new();
    collect_tree_paths_for_commit(repo, commit.tree, "", &mut paths)?;
    Ok(paths)
}

fn collect_tree_paths_for_commit(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    out: &mut HashSet<Vec<u8>>,
) -> Result<()> {
    let obj = repo.odb.read(&tree_oid)?;
    for entry in parse_tree(&obj.data)? {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == MODE_TREE {
            collect_tree_paths_for_commit(repo, entry.oid, &path, out)?;
        } else {
            out.insert(path.into_bytes());
        }
    }
    Ok(())
}

#[allow(dead_code)]
enum AddPathError {
    Ignored(String),
    IoError(anyhow::Error),
    Other(anyhow::Error),
    /// New file path is outside the sparse-checkout definition (needs advice + exit 1).
    OutsideSparse(String),
    /// Nested `.git` with no resolvable HEAD commit (matches Git's add error shape).
    EmbeddedNoCommit {
        rel_path: String,
    },
    /// `git add --dry-run`: missing pathspec — print `fatal: ...` on stdout and exit 128.
    DryRunFatalPathspec {
        message: String,
    },
}

impl From<anyhow::Error> for AddPathError {
    fn from(e: anyhow::Error) -> Self {
        AddPathError::Other(e)
    }
}

/// Run --refresh: update stat info in the index.
/// Refresh a single stage-0 index entry's cached stat data from the work tree.
///
/// Mirrors Git's `refresh_index` for one entry: updates ctime/mtime/dev/ino/uid/gid/size
/// from `fs::symlink_metadata`. Returns `true` when the work-tree file existed and the
/// entry was refreshed.
fn refresh_index_entry(ie: &mut IndexEntry, abs_path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(abs_path) else {
        return false;
    };
    ie.ctime_sec = meta.ctime() as u32;
    ie.ctime_nsec = meta.ctime_nsec() as u32;
    ie.mtime_sec = meta.mtime() as u32;
    ie.mtime_nsec = meta.mtime_nsec() as u32;
    ie.dev = meta.dev() as u32;
    ie.ino = meta.ino() as u32;
    ie.uid = meta.uid();
    ie.gid = meta.gid();
    ie.size = meta.len() as u32;
    true
}

#[allow(clippy::too_many_arguments)]
fn run_refresh(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    prefix: Option<&str>,
    args: &Args,
    sparse: &AddSparseState,
    config: &ConfigSet,
) -> Result<()> {
    // Git's `refresh()` passes `REFRESH_IGNORE_SKIP_WORKTREE`: `refresh_index` itself only
    // skips entries with the skip-worktree bit set (an out-of-cone path whose work-tree file
    // exists is still refreshed and counts as "seen"). Afterwards, any pathspec that matched
    // no refreshed entry but does match a skip-worktree or out-of-cone path triggers the
    // sparse-path advice and a non-zero exit without touching cached stat data. `--sparse`
    // disables both guards.
    let include_sparse = args.sparse;

    if args.pathspec.is_empty() {
        // Refresh all entries
        for ie in &mut index.entries {
            if ie.stage() != 0 {
                continue;
            }
            // REFRESH_IGNORE_SKIP_WORKTREE: never refresh skip-worktree entries.
            if !include_sparse && ie.skip_worktree() {
                continue;
            }
            let path_str = String::from_utf8_lossy(&ie.path).to_string();
            if let Some(p) = prefix {
                if !path_str.starts_with(p) {
                    continue;
                }
            }
            let abs_path = work_tree.join(&path_str);
            refresh_index_entry(ie, &abs_path);
        }
    } else {
        let mut only_match_sparse: Vec<String> = Vec::new();
        for pathspec in &args.pathspec {
            // Did the pathspec match an entry that `refresh_index` actually refreshed?
            let mut seen = false;
            // Did it match a skip-worktree or out-of-cone path we declined to update?
            let mut matched_sparse = false;
            for ie in &mut index.entries {
                if ie.stage() != 0 {
                    continue;
                }
                let path_str = String::from_utf8_lossy(&ie.path).to_string();
                if !grit_lib::pathspec::pathspec_matches(pathspec, &path_str) {
                    continue;
                }
                // REFRESH_IGNORE_SKIP_WORKTREE: skip-worktree entries are never refreshed.
                if !include_sparse && ie.skip_worktree() {
                    matched_sparse = true;
                    continue;
                }
                // An out-of-cone path with no work-tree file cannot be refreshed; classify
                // it as sparse (mirrors `!path_in_sparse_checkout` in Git's `refresh()`).
                if !include_sparse && sparse.add_update_blocked(false, Some(ie), &path_str) {
                    matched_sparse = true;
                }
                let abs_path = work_tree.join(&path_str);
                if refresh_index_entry(ie, &abs_path) {
                    seen = true;
                }
            }
            if seen {
                continue;
            }
            if matched_sparse {
                only_match_sparse.push(pathspec.clone());
            } else if !args.ignore_missing {
                // Git's `refresh()` dies with this exact wording (builtin/add.c).
                eprintln!("fatal: pathspec '{}' did not match any files", pathspec);
                std::process::exit(128);
            }
        }

        if !only_match_sparse.is_empty() {
            only_match_sparse.sort();
            only_match_sparse.dedup();
            emit_sparse_path_advice(&mut std::io::stderr(), config, &only_match_sparse)?;
            // The cached stat data of sparse entries is left untouched; the index may still
            // need rewriting for any non-sparse entries we did refresh.
            if !args.dry_run {
                write_index_or_lock_err(repo, index, &resolved_env_index_path(repo))?;
            }
            std::process::exit(1);
        }
    }

    if !args.dry_run {
        write_index_or_lock_err(repo, index, &resolved_env_index_path(repo))?;
    }

    Ok(())
}

/// Re-apply clean conversion (CRLF normalization) to tracked files.
fn run_renormalize(
    repo: &Repository,
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    prefix: Option<&str>,
    args: &Args,
    add_cfg: &AddConfig,
) -> Result<()> {
    // Reload gitattributes (may have been updated)
    let attrs = crlf::load_gitattributes(work_tree);

    // Collect paths to renormalize based on pathspecs
    let entries: Vec<(Vec<u8>, ObjectId, u32)> = index
        .entries
        .iter()
        .filter(|ie| {
            if ie.stage() != 0 {
                return false;
            }
            let path_str = String::from_utf8_lossy(&ie.path);
            if add_cfg.sparse.add_update_blocked(
                add_cfg.include_sparse,
                Some(ie),
                path_str.as_ref(),
            ) {
                return false;
            }
            if args.pathspec.is_empty() {
                return true;
            }
            args.pathspec.iter().any(|ps| {
                let ps_clean = ps.trim_end_matches('*').trim_end_matches('/');
                path_str.starts_with(ps_clean) || glob_matches_simple(ps, &path_str)
            })
        })
        .map(|ie| (ie.path.clone(), ie.oid, ie.mode))
        .collect();

    for (path, oid, mode) in entries {
        let rel_path = String::from_utf8_lossy(&path).to_string();
        let file_attrs = crlf::get_file_attrs(&attrs, &rel_path, false, &add_cfg.config);

        let wt_path = work_tree.join(&rel_path);
        let (prior_blob, raw_for_clean): (Vec<u8>, Vec<u8>) = if mode == 0o120000 {
            let target = fs::read_link(&wt_path)
                .with_context(|| format!("reading symlink for renormalize '{rel_path}'"))?;
            let raw = target.to_string_lossy().into_owned().into_bytes();
            let obj = odb.read(&oid).context("reading blob for renormalize")?;
            if obj.kind != ObjectKind::Blob {
                continue;
            }
            (obj.data, raw)
        } else if fs::symlink_metadata(&wt_path).is_ok() {
            let raw = fs::read(&wt_path)
                .with_context(|| format!("reading work tree for renormalize '{rel_path}'"))?;
            let obj = odb.read(&oid).context("reading blob for renormalize")?;
            if obj.kind != ObjectKind::Blob {
                continue;
            }
            (obj.data, raw)
        } else {
            let obj = odb.read(&oid).context("reading blob for renormalize")?;
            if obj.kind != ObjectKind::Blob {
                continue;
            }
            let d = obj.data.clone();
            (d.clone(), d)
        };

        let converted =
            match crlf::convert_to_git(&raw_for_clean, &rel_path, &add_cfg.conv, &file_attrs) {
                Ok(c) => c,
                Err(_) => continue,
            };

        if converted != prior_blob {
            let new_oid = odb.write(ObjectKind::Blob, &converted)?;
            if let Some(entry) = index.get_mut(path.as_slice(), 0) {
                entry.oid = new_oid;
            }
        }
    }

    if !add_cfg.include_sparse {
        let mut sparse_renorm: Vec<String> = Vec::new();
        let mut ignore_matcher = IgnoreMatcher::from_repository(repo).ok();
        for spec in &args.pathspec {
            let resolved = crate::pathspec::resolve_pathspec(spec, work_tree, prefix);
            if pathspec_only_matches_sparse_blocked(
                spec,
                &resolved,
                index,
                work_tree,
                &add_cfg.sparse,
                add_cfg.precompose_unicode,
                repo,
                &mut ignore_matcher,
            ) {
                sparse_renorm.push(spec.clone());
            }
        }
        if !sparse_renorm.is_empty() {
            sparse_renorm.sort();
            sparse_renorm.dedup();
            emit_sparse_path_advice(&mut std::io::stderr(), &add_cfg.config, &sparse_renorm)?;
            if !args.dry_run {
                let index_path = resolved_env_index_path(repo);
                write_index_or_lock_err(repo, index, &index_path)?;
            }
            std::process::exit(1);
        }
    }

    if !args.dry_run {
        let index_path = resolved_env_index_path(repo);
        write_index_or_lock_err(repo, index, &index_path)?;
    }

    Ok(())
}

fn glob_matches_simple(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return text == pattern || text.starts_with(&format!("{pattern}/"));
    }
    // Simple glob: *.txt
    if let Some(suffix) = pattern.strip_prefix('*') {
        return text.ends_with(suffix);
    }
    text == pattern
}

fn resolved_pathspecs_for_add(
    pathspecs: &[String],
    work_tree: &Path,
    prefix: Option<&str>,
) -> Result<Vec<String>> {
    pathspecs
        .iter()
        .map(|s| {
            crate::pathspec::resolve_pathspec_in_worktree(s, s, work_tree, prefix)
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .collect()
}

fn pathspecs_are_all_exclude(pathspecs: &[String]) -> bool {
    !pathspecs.is_empty()
        && pathspecs
            .iter()
            .all(|s| grit_lib::pathspec::pathspec_is_exclude(s))
}

fn pathspecs_need_match_walk(pathspecs: &[String]) -> bool {
    pathspecs.iter().any(|s| {
        if grit_lib::pathspec::pathspec_is_exclude(s) {
            return false;
        }
        if pathspec_uses_long_magic(s) {
            return true;
        }
        // A wildcard pathspec like `*.c` matches at any directory depth (git uses wildmatch without
        // `WM_PATHNAME`, so `*` crosses `/`). The shallow `expand_glob_pathspec` only reads one
        // directory level, so route any wildcard spec through the recursive worktree walk +
        // `matches_pathspec_list` matcher instead (t3701 "add -p handles globs").
        grit_lib::pathspec::has_glob_chars(s)
    })
}

fn pathspecs_use_attr_magic(pathspecs: &[String]) -> bool {
    pathspecs
        .iter()
        .any(|spec| spec.starts_with(":(attr:") || spec.contains(",attr:"))
}

fn add_pathspec_matches_worktree(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    rel_path: &str,
    pathspecs: &[String],
) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    if !pathspecs_use_attr_magic(pathspecs) {
        return grit_lib::pathspec::matches_pathspec_list(rel_path, pathspecs);
    }
    let attrs =
        grit_lib::crlf::load_gitattributes_for_checkout(work_tree, rel_path, index, &repo.odb);
    let mode = fs::symlink_metadata(work_tree.join(rel_path))
        .map(|meta| normalize_mode(meta.mode()))
        .unwrap_or(0);
    grit_lib::pathspec::matches_pathspec_list_for_object(rel_path, mode, &attrs, pathspecs)
}

fn add_pathspec_matches_index(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    rel_path: &str,
    mode: u32,
    pathspecs: &[String],
) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    if !pathspecs_use_attr_magic(pathspecs) {
        return grit_lib::pathspec::matches_pathspec_list(rel_path, pathspecs);
    }
    let attrs =
        grit_lib::crlf::load_gitattributes_for_checkout(work_tree, rel_path, index, &repo.odb);
    grit_lib::pathspec::matches_pathspec_list_for_object(rel_path, mode, &attrs, pathspecs)
}

fn pathspec_uses_long_magic(pathspec: &str) -> bool {
    !grit_lib::pathspec::literal_pathspecs_enabled()
        && pathspec
            .strip_prefix(":(")
            .is_some_and(|rest| rest.contains(')'))
}

/// Stage every work-tree file that matches `pathspecs` (Git `match_pathspec`, including excludes).
fn add_with_pathspec_list(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    pathspecs: &[String],
    args: &Args,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
    add_cfg: &AddConfig,
) -> Result<bool> {
    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    walk_directory(
        work_tree,
        work_tree,
        &mut paths,
        repo,
        ignore_matcher,
        args.force,
        add_cfg.precompose_unicode,
    )?;

    let worktree_paths: std::collections::HashSet<&str> =
        paths.iter().map(|(r, _)| r.as_str()).collect();
    let mut matched_any = false;

    for (rel_path, abs_path) in &paths {
        if !add_pathspec_matches_worktree(repo, index, work_tree, rel_path, pathspecs) {
            continue;
        }
        matched_any = true;
        if let Err(e) = stage_file(
            odb,
            index,
            work_tree,
            rel_path,
            abs_path,
            repo,
            &StageFileContext::from(args),
            add_cfg,
        ) {
            if add_cfg.ignore_errors {
                eprintln!("warning: {e}");
            } else {
                return Err(e);
            }
        }
    }

    let removed: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|ie| {
            if ie.skip_worktree() {
                return false;
            }
            let path_str = std::str::from_utf8(&ie.path).unwrap_or("");
            add_pathspec_matches_index(repo, index, work_tree, path_str, ie.mode, pathspecs)
                && !worktree_paths.contains(path_str)
        })
        .map(|ie| ie.path.clone())
        .collect();

    for path in removed {
        matched_any = true;
        if args.verbose {
            let path_str = String::from_utf8_lossy(&path);
            eprintln!("remove '{path_str}'");
        }
        if !args.dry_run {
            index.remove(&path);
        }
    }

    Ok(matched_any)
}

/// Add all files under the working tree (or a prefix) to the index.
fn add_all(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    prefix: Option<&str>,
    pathspecs: &[String],
    args: &Args,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
    add_cfg: &AddConfig,
    sparse_advice_paths: &mut Vec<String>,
) -> Result<(bool, Option<anyhow::Error>)> {
    let scan_root = match prefix {
        Some(p) if !p.is_empty() => work_tree.join(p),
        _ => work_tree.to_path_buf(),
    };

    let mut skipped_outside_sparse = false;
    let mut matched_any = false;
    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    walk_directory(
        &scan_root,
        work_tree,
        &mut paths,
        repo,
        ignore_matcher,
        args.force,
        add_cfg.precompose_unicode,
    )?;
    paths.sort_by(|a, b| a.0.cmp(&b.0));

    let mut chmod_err: Option<anyhow::Error> = None;
    if !args.dry_run {
        let mut ignored_some = false;
        for (rel_path, abs_path) in &paths {
            if !add_pathspec_matches_worktree(repo, index, work_tree, rel_path, pathspecs) {
                continue;
            }
            if add_cfg.sparse.sparse_enabled
                && !add_cfg.include_sparse
                && !add_cfg.sparse.path_in_sparse_definition(rel_path.as_str())
            {
                skipped_outside_sparse = true;
                continue;
            }
            if let Err(e) = stage_file(
                odb,
                index,
                work_tree,
                rel_path,
                abs_path,
                repo,
                &StageFileContext::from(args),
                add_cfg,
            ) {
                if add_cfg.ignore_errors {
                    eprintln!("warning: {e}");
                    ignored_some = true;
                } else if args.chmod.is_some() && error_is_chmod_on_non_regular(&e) {
                    chmod_err = chmod_err.or(Some(e));
                } else {
                    return Err(e);
                }
            } else {
                matched_any = true;
            }
        }
        if add_cfg.ignore_errors && ignored_some {
            return Ok((true, chmod_err));
        }
    }

    // Build a set of worktree paths for fast deletion detection
    let worktree_paths: std::collections::HashSet<&str> =
        paths.iter().map(|(r, _)| r.as_str()).collect();

    // Handle deletions: index entries whose files are not in the worktree scan
    let prefix_bytes = prefix.map(|p| p.as_bytes());
    let removed: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|ie| {
            if let Some(pb) = prefix_bytes {
                if !index_path_under_prefix(&ie.path, pb) {
                    return false;
                }
            }
            // In sparse-checkout mode, entries outside the sparse view are
            // marked skip-worktree and may legitimately be absent from the
            // working tree. Do not treat those as deletions for `git add .`.
            if ie.skip_worktree() {
                return false;
            }
            let path_str = std::str::from_utf8(&ie.path).unwrap_or("");
            if !add_pathspec_matches_index(repo, index, work_tree, path_str, ie.mode, pathspecs) {
                return false;
            }
            !worktree_paths.contains(path_str)
        })
        .map(|ie| ie.path.clone())
        .collect();

    for path in removed {
        matched_any = true;
        if args.verbose {
            let path_str = String::from_utf8_lossy(&path);
            eprintln!("remove '{path_str}'");
        }
        if !args.dry_run {
            index.remove(&path);
        }
    }

    if !matched_any
        && args.pathspec.iter().any(|s| s == ".")
        && prefix.map(|p| p.is_empty()).unwrap_or(true)
        && skipped_outside_sparse
        && !add_cfg.include_sparse
    {
        sparse_advice_paths.push(".".to_string());
    }

    if args.dry_run {
        for (rel_path, abs_path) in &paths {
            if !add_pathspec_matches_worktree(repo, index, work_tree, rel_path, pathspecs) {
                continue;
            }
            if add_cfg.sparse.sparse_enabled
                && !add_cfg.sparse.path_in_sparse_definition(rel_path.as_str())
            {
                continue;
            }
            stage_file(
                odb,
                index,
                work_tree,
                rel_path,
                abs_path,
                repo,
                &StageFileContext::from(args),
                add_cfg,
            )?;
        }
    }

    Ok((false, chmod_err))
}

/// True when `path` (index UTF-8 path) is exactly `prefix` or under `prefix/` (path component boundary).
fn index_path_under_prefix(path: &[u8], prefix: &[u8]) -> bool {
    if path == prefix {
        return true;
    }
    path.len() > prefix.len() && path.starts_with(prefix) && path[prefix.len()] == b'/'
}

fn path_matches_any_resolved_spec(path: &str, specs: &[String]) -> bool {
    specs
        .iter()
        .any(|s| path == s.as_str() || path.starts_with(&format!("{s}/")))
}

/// `git add -A <pathspec>...` — stage updates only under the given pathspecs and record deletions
/// there (not the whole tree). Matches Git when path arguments are present with `-A`.
fn add_all_for_pathspecs(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    cwd_prefix: Option<&str>,
    pathspecs: &[String],
    args: &Args,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
    add_cfg: &AddConfig,
    sparse_advice_out: &mut Vec<String>,
) -> Result<()> {
    let mut resolved_specs: Vec<String> = Vec::new();

    for ps in pathspecs {
        let resolved = crate::pathspec::resolve_pathspec(ps, work_tree, cwd_prefix);
        let expanded = expand_glob_pathspec(&resolved, work_tree, add_cfg.precompose_unicode);
        for r in expanded {
            resolved_specs.push(r);
        }
    }

    resolved_specs.sort();
    resolved_specs.dedup();

    let mut had_ignored = false;
    let mut had_errors = false;
    let mut any_staged = false;
    for r in &resolved_specs {
        match add_path(
            odb,
            index,
            work_tree,
            r,
            args,
            repo,
            ignore_matcher,
            add_cfg,
        ) {
            Ok(()) => {
                any_staged = true;
            }
            Err(AddPathError::Ignored(msg)) => {
                eprintln!("{msg}");
                had_ignored = true;
                had_errors = true;
            }
            Err(AddPathError::IoError(e)) => {
                if add_cfg.ignore_errors {
                    eprintln!("warning: {e}");
                    had_errors = true;
                } else {
                    return Err(e);
                }
            }
            Err(AddPathError::Other(e)) => {
                let s = e.to_string();
                if pathspecs.len() > 1
                    && (s.contains("did not match any files")
                        || s.contains("did not match any file"))
                {
                    continue;
                }
                if add_cfg.ignore_errors {
                    eprintln!("warning: {e}");
                    had_errors = true;
                } else {
                    return Err(e);
                }
            }
            Err(AddPathError::OutsideSparse(path)) => {
                sparse_advice_out.push(path);
            }
            Err(AddPathError::DryRunFatalPathspec { message }) => {
                println!("{message}");
                return Err(anyhow::Error::new(SilentNonZeroExit { code: 128 }));
            }
            Err(AddPathError::EmbeddedNoCommit { rel_path }) => {
                eprintln!("error: '{rel_path}' does not have a commit checked out");
                eprintln!("error: unable to index file '{rel_path}'");
                eprintln!("fatal: adding files failed");
                std::process::exit(128);
            }
        }
    }

    if !any_staged && !had_ignored && !had_errors {
        if !sparse_advice_out.is_empty() {
            // Sparse-checkout advice only; outer `run` emits and exits.
            return Ok(());
        }
        bail!(
            "pathspec '{}' did not match any file(s) known to git",
            pathspecs.join(" ")
        );
    }

    if had_ignored {
        bail!("some ignored files could not be added");
    }
    if had_errors && !add_cfg.ignore_errors {
        bail!("adding files failed");
    }

    let to_remove: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|ie| {
            if ie.stage() != 0 {
                return false;
            }
            if ie.skip_worktree() {
                return false;
            }
            let path_str = std::str::from_utf8(&ie.path).unwrap_or("");
            if !path_matches_any_resolved_spec(path_str, &resolved_specs) {
                return false;
            }
            if ie.mode == 0o160000
                && submodule_ignore_all_for_add(repo, work_tree, path_str)
                && work_tree.join(path_str).join(".git").exists()
            {
                return false;
            }
            fs::symlink_metadata(work_tree.join(path_str)).is_err()
        })
        .map(|ie| ie.path.clone())
        .collect();

    for path in to_remove {
        if args.verbose {
            let path_str = String::from_utf8_lossy(&path);
            eprintln!("remove '{path_str}'");
        }
        if !args.dry_run {
            index.remove(&path);
        }
    }

    Ok(())
}

/// Update only already-tracked files.
fn update_tracked(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    prefix: Option<&str>,
    args: &Args,
    repo: &Repository,
    add_cfg: &AddConfig,
    sparse_advice_paths: &mut Vec<String>,
) -> Result<()> {
    // If explicit pathspecs given with -u, validate that each matches a tracked file.
    let explicit_pathspecs = !args.pathspec.is_empty();
    let resolved_pathspecs = if explicit_pathspecs {
        resolved_pathspecs_for_add(&args.pathspec, work_tree, prefix)?
    } else {
        Vec::new()
    };
    if explicit_pathspecs && !pathspecs_are_all_exclude(&resolved_pathspecs) {
        let pfx = prefix.unwrap_or("");
        for spec in &args.pathspec {
            // Build the full path as it would appear in the index
            let full_spec = if pfx.is_empty() || spec.starts_with('/') {
                spec.clone()
            } else {
                format!("{pfx}/{spec}")
            };
            let matches_tracked = spec == "."
                || spec.is_empty()
                || index.entries.iter().any(|ie| {
                    let p = String::from_utf8_lossy(&ie.path);
                    p == full_spec.as_str()
                        || p.starts_with(&format!("{full_spec}/"))
                        || p == spec.as_str()
                        || p.starts_with(&format!("{spec}/"))
                });
            if !matches_tracked {
                eprintln!("error: pathspec '{spec}' did not match any file(s) known to git");
                std::process::exit(128);
            }
        }
    }

    let tracked: Vec<(Vec<u8>, String)> = index
        .entries
        .iter()
        .filter(|ie| {
            let path_str = String::from_utf8_lossy(&ie.path);
            // Apply prefix filter ONLY when explicit pathspecs are given.
            // Without explicit pathspecs, git add -u updates ALL tracked files from root.
            let prefix_ok = if explicit_pathspecs {
                prefix.map(|p| path_str.starts_with(p)).unwrap_or(true)
            } else {
                true // update everything from root
            };
            // Apply explicit pathspec filter
            let pathspec_ok = if explicit_pathspecs {
                add_pathspec_matches_index(
                    repo,
                    index,
                    work_tree,
                    path_str.as_ref(),
                    ie.mode,
                    &resolved_pathspecs,
                )
            } else {
                true
            };
            prefix_ok && pathspec_ok
        })
        .map(|ie| {
            let path_str = String::from_utf8_lossy(&ie.path).to_string();
            (ie.path.clone(), path_str)
        })
        .collect();

    for (raw_path, path_str) in &tracked {
        let ie = index.get(raw_path.as_slice(), 0);
        if add_cfg
            .sparse
            .add_update_blocked(add_cfg.include_sparse, ie, path_str)
        {
            continue;
        }
        let abs_path = work_tree.join(path_str);
        if path_has_symlink_parent_for_add(work_tree, &abs_path) {
            if args.verbose || args.dry_run {
                dry_run_stdout_push_line(format!("remove '{path_str}'"));
            }
            if !args.dry_run {
                index.remove(raw_path);
            }
            continue;
        }
        // Use symlink_metadata so a symlink whose target was removed still counts as
        // present (`exists()` follows the link and returns false).
        if let Ok(meta) = fs::symlink_metadata(&abs_path) {
            // A tracked blob replaced by a plain directory: `git add -u` only drops the index entry
            // so a later `git add` can record the directory (matches git). Embedded repos still stage
            // as gitlinks via `stage_file`.
            let is_plain_directory = meta.is_dir() && !meta.file_type().is_symlink();
            let is_embedded_repo = abs_path.join(".git").exists();
            if is_plain_directory && !is_embedded_repo {
                if args.verbose || args.dry_run {
                    dry_run_stdout_push_line(format!("remove '{path_str}'"));
                }
                if !args.dry_run {
                    index.remove(raw_path);
                }
            } else if args.dry_run {
                // For dry-run, hash without writing to ODB
                if let Ok(data) = std::fs::read(&abs_path) {
                    let oid = grit_lib::odb::Odb::hash_object_data(
                        grit_lib::objects::ObjectKind::Blob,
                        &data,
                    );
                    let current = index.get(raw_path, 0);
                    if current.map(|e| e.oid != oid).unwrap_or(false) {
                        dry_run_stdout_push_line(format!("add '{path_str}'"));
                    }
                }
            } else {
                stage_file(
                    odb,
                    index,
                    work_tree,
                    path_str,
                    &abs_path,
                    repo,
                    &StageFileContext::from(args),
                    add_cfg,
                )?;
            }
        } else {
            if args.verbose || args.dry_run {
                dry_run_stdout_push_line(format!("remove '{path_str}'"));
            }
            if !args.dry_run {
                index.remove(raw_path);
            }
        }
    }

    if explicit_pathspecs && !add_cfg.include_sparse {
        let mut ignore_matcher = IgnoreMatcher::from_repository(repo).ok();
        for spec in &args.pathspec {
            let resolved = crate::pathspec::resolve_pathspec(spec, work_tree, prefix);
            if pathspec_only_matches_sparse_blocked(
                spec,
                &resolved,
                index,
                work_tree,
                &add_cfg.sparse,
                add_cfg.precompose_unicode,
                repo,
                &mut ignore_matcher,
            ) {
                sparse_advice_paths.push(spec.clone());
            }
        }
    }

    Ok(())
}

/// Resolve `rel` to the spelling that exists on disk when NFC/NFD differ (Linux + precompose).
fn resolve_add_path_on_disk(
    work_tree: &Path,
    rel: &str,
    precompose_unicode: bool,
) -> (PathBuf, String) {
    let abs = work_tree.join(rel);
    if fs::symlink_metadata(&abs).is_ok() {
        return (abs, rel.to_owned());
    }
    if !precompose_unicode {
        return (abs, rel.to_owned());
    }
    let p = Path::new(rel);
    let want_leaf = precompose_utf8_path(
        p.file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default()
            .as_ref(),
    )
    .into_owned();
    if want_leaf.is_empty() {
        return (abs, rel.to_owned());
    }
    let parent_rel = p.parent().filter(|x| !x.as_os_str().is_empty());
    let parent_abs = parent_rel
        .map(|pr| work_tree.join(pr))
        .unwrap_or_else(|| work_tree.to_path_buf());
    if let Ok(rd) = fs::read_dir(&parent_abs) {
        for ent in rd.flatten() {
            let n = ent.file_name().to_string_lossy().into_owned();
            if precompose_utf8_path(&n).as_ref() == want_leaf.as_str() {
                let new_rel = match parent_rel {
                    Some(pr) => format!("{}/{}", pr.to_string_lossy(), n),
                    None => n,
                };
                return (work_tree.join(&new_rel), new_rel);
            }
        }
    }
    (abs, rel.to_owned())
}

/// Add a single pathspec (which may be a file or directory).
fn add_path(
    odb: &Odb,
    index: &mut Index,
    work_tree: &Path,
    path: &str,
    args: &Args,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
    add_cfg: &AddConfig,
) -> std::result::Result<(), AddPathError> {
    let (abs_path, path_on_disk) =
        resolve_add_path_on_disk(work_tree, path, add_cfg.precompose_unicode);
    let path = path_on_disk.as_str();

    // Refuse to add a path inside a registered submodule (gitlink).
    // Only reject when a *proper* parent directory is a gitlink;
    // adding the submodule entry itself (e.g. `git add embed`) is fine.
    {
        let path_for_parent_check = path.trim_end_matches('/');
        let components: Vec<&str> = path_for_parent_check.split('/').collect();
        let mut prefix = String::new();
        for &component in &components[..components.len().saturating_sub(1)] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if let Some(ie) = index.get(prefix.as_bytes(), 0) {
                if ie.mode == 0o160000 {
                    eprintln!("fatal: Pathspec '{}' is in submodule '{}'", path, prefix);
                    std::process::exit(128);
                }
            }
        }
    }

    // Refuse to add a path that traverses through a symbolic link.
    if check_symlink_in_path(work_tree, Path::new(path)).is_some() {
        return Err(AddPathError::Other(anyhow::anyhow!(
            "'{}' is beyond a symbolic link",
            path
        )));
    }

    // Use symlink_metadata to detect dangling symlinks (exists() follows symlinks)
    if fs::symlink_metadata(&abs_path).is_err() {
        let path_bytes = path.as_bytes();
        // Check if it's an index entry that needs to be removed
        if let Some(ie) = index.get(path_bytes, 0) {
            if add_cfg
                .sparse
                .add_update_blocked(args.sparse, Some(ie), path)
            {
                return Ok(());
            }
            if !args.dry_run {
                index.remove(path_bytes);
            }
            if args.verbose {
                eprintln!("remove '{path}'");
            }
            return Ok(());
        }
        // Check unmerged entries (stages 1, 2, 3)
        let has_unmerged = (1..=3).any(|stage| index.get(path_bytes, stage).is_some());
        if has_unmerged {
            // Can't resolve a conflict if file doesn't exist. Git uses die()
            // here (`fatal:` + exit 128); prefix with `fatal: ` so main.rs
            // re-emits the message verbatim and exits 128. The multi-pathspec
            // arm still matches "did not match any files" to `continue`.
            return Err(AddPathError::Other(anyhow::anyhow!(
                "fatal: pathspec '{}' did not match any files",
                path
            )));
        }
        if args.dry_run && args.ignore_missing {
            if !args.force {
                if let Some(ref mut matcher) = ignore_matcher {
                    let (is_ignored, _) = matcher
                        .check_path(repo, Some(&*index), path, false)
                        .map_err(|e| AddPathError::Other(e.into()))?;
                    if is_ignored {
                        return Err(AddPathError::Ignored(format!(
                            "The following paths are ignored by one of your .gitignore files:\n\
                             {path}\n\
                             hint: Use -f if you really want to add them.\n\
                             hint: Disable this message with \"git config set advice.addIgnoredFile false\""
                        )));
                    }
                }
            }
            return Ok(());
        }
        if args.dry_run && DRY_RUN_CAPTURE_MULTISPEC.get() {
            dry_run_stdout_abort_multispec_capture();
            return Err(AddPathError::DryRunFatalPathspec {
                message: format!("fatal: pathspec '{path}' did not match any files"),
            });
        }
        // Git die()s here (`fatal:` + exit 128). Prefix so main.rs re-emits
        // the message verbatim and exits 128; the multi-pathspec arm still
        // matches "did not match any files" to `continue`.
        return Err(AddPathError::Other(anyhow::anyhow!(
            "fatal: pathspec '{}' did not match any files",
            path
        )));
    }

    // Use symlink_metadata so symlinks to directories are staged as
    // symlinks, not traversed.
    let is_real_dir = fs::symlink_metadata(&abs_path)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false);

    if is_real_dir {
        // Check if the directory itself is ignored (reject unless -f)
        if !args.force {
            if let Some(ref mut matcher) = ignore_matcher {
                // Check the directory path (with trailing slash for dir matching)
                let dir_path_slash = format!("{path}/");
                let (is_ignored, _) = matcher
                    .check_path(repo, Some(&*index), path, true)
                    .or_else(|_| matcher.check_path(repo, Some(&*index), &dir_path_slash, true))
                    .unwrap_or((false, None));
                if is_ignored {
                    return Err(AddPathError::Ignored(format!(
                        "The following paths are ignored by one of your .gitignore files:\n\
                         {path}\n\
                         Use -f if you really want to add them."
                    )));
                }
            }
        }

        // Nested repository (subdirectory with its own .git, not the superproject root).
        if is_nested_embedded_git_repo(&abs_path, repo) {
            let gitlink_path = path
                .trim_end_matches('/')
                .strip_prefix("./")
                .unwrap_or_else(|| path.trim_end_matches('/'));
            if !args.force && submodule_ignore_all_for_add(repo, work_tree, gitlink_path) {
                println!("Skipping submodule due to ignore=all: {gitlink_path}");
                return Ok(());
            }
            return stage_gitlink(
                odb,
                index,
                repo,
                gitlink_path,
                &abs_path,
                args.dry_run,
                args.verbose,
                args.no_warn_embedded_repo || index.get(gitlink_path.as_bytes(), 0).is_some(),
            )
            .map_err(|e| {
                if e.downcast_ref::<EmbeddedRepoNoCommitError>().is_some() {
                    AddPathError::EmbeddedNoCommit {
                        rel_path: format!("{}/", path.trim_end_matches('/')),
                    }
                } else {
                    AddPathError::IoError(e)
                }
            });
        }

        let mut paths: Vec<(String, PathBuf)> = Vec::new();
        walk_directory(
            &abs_path,
            work_tree,
            &mut paths,
            repo,
            ignore_matcher,
            args.force,
            add_cfg.precompose_unicode,
        )?;
        let worktree_paths: std::collections::HashSet<&str> =
            paths.iter().map(|(r, _)| r.as_str()).collect();
        for (rel_path, file_abs) in &paths {
            if let Err(e) = stage_file(
                odb,
                index,
                work_tree,
                rel_path,
                file_abs,
                repo,
                &StageFileContext::from(args),
                add_cfg,
            ) {
                if let Some(a) = e.downcast_ref::<AddOutsideSparse>() {
                    return Err(AddPathError::OutsideSparse(a.path.clone()));
                }
                if add_cfg.ignore_errors {
                    eprintln!("warning: {e}");
                } else {
                    return Err(AddPathError::IoError(e));
                }
            }
        }

        // `git add <dir>` also stages deletions: index entries under this
        // directory whose working-tree file no longer exists are removed.
        let dir_prefix = {
            let trimmed = path.trim_end_matches('/').trim_start_matches("./");
            if trimmed.is_empty() {
                String::new()
            } else {
                format!("{trimmed}/")
            }
        };
        let removed: Vec<Vec<u8>> = index
            .entries
            .iter()
            .filter(|ie| {
                if ie.stage() != 0 || ie.skip_worktree() {
                    return false;
                }
                let Ok(p) = std::str::from_utf8(&ie.path) else {
                    return false;
                };
                if !(dir_prefix.is_empty() || p.starts_with(&dir_prefix)) {
                    return false;
                }
                if worktree_paths.contains(p) {
                    return false;
                }
                // Only remove if the working-tree file genuinely no longer
                // exists (a tracked-but-ignored present file must be kept).
                fs::symlink_metadata(work_tree.join(p)).is_err()
            })
            .map(|ie| ie.path.clone())
            .collect();
        for p in removed {
            if !args.dry_run {
                index.remove(&p);
            }
            if args.verbose {
                let path_str = String::from_utf8_lossy(&p);
                eprintln!("remove '{path_str}'");
            }
        }

        let removed_unmerged: HashSet<Vec<u8>> = index
            .entries
            .iter()
            .filter(|ie| {
                if ie.stage() == 0 {
                    return false;
                }
                let Ok(p) = std::str::from_utf8(&ie.path) else {
                    return false;
                };
                if !(dir_prefix.is_empty() || p.starts_with(&dir_prefix)) {
                    return false;
                }
                if worktree_paths.contains(p) {
                    return false;
                }
                fs::symlink_metadata(work_tree.join(p)).is_err()
            })
            .map(|ie| ie.path.clone())
            .collect();
        for p in removed_unmerged {
            if !args.dry_run {
                index.remove(&p);
            }
            if args.verbose {
                let path_str = String::from_utf8_lossy(&p);
                eprintln!("remove '{path_str}'");
            }
        }
    } else {
        // Allow adding ignored files when resolving merge conflicts (unmerged entries).
        let path_bytes = path.as_bytes();
        let has_unmerged = (1..=3).any(|stage| index.get(path_bytes, stage).is_some());

        // Check ignore patterns for explicitly named files (like real git),
        // but skip the check if the file has unmerged entries (conflict resolution).
        if !has_unmerged {
            if let Some(ref mut matcher) = ignore_matcher {
                let (is_ignored, _match_info) = matcher
                    .check_path(repo, Some(&*index), path, false)
                    .map_err(|e| AddPathError::Other(e.into()))?;
                if is_ignored {
                    return Err(AddPathError::Ignored(format!(
                        "The following paths are ignored by one of your .gitignore files:\n\
                         {path}\n\
                         Use -f if you really want to add them."
                    )));
                }
            }
        }
        stage_file(
            odb,
            index,
            work_tree,
            path,
            &abs_path,
            repo,
            &StageFileContext::from(args),
            add_cfg,
        )
        .map_err(|e| {
            if let Some(a) = e.downcast_ref::<AddOutsideSparse>() {
                AddPathError::OutsideSparse(a.path.clone())
            } else {
                AddPathError::IoError(e)
            }
        })?;
    }

    Ok(())
}

/// Resolve the Git directory for an embedded repository work tree.
///
/// Supports a `.git` directory or a `.git` file with `gitdir:` (submodule-style layout).
fn embedded_repository_git_dir(worktree: &Path) -> Result<PathBuf> {
    let dot_git = worktree.join(".git");
    let meta = fs::symlink_metadata(&dot_git)
        .with_context(|| format!("cannot stat .git in embedded repo {}", worktree.display()))?;
    if meta.file_type().is_dir() {
        return Ok(dot_git);
    }
    let content = fs::read_to_string(&dot_git)
        .with_context(|| format!("cannot read .git file in {}", worktree.display()))?;
    let line = content.lines().next().unwrap_or("").trim();
    let rest = line.strip_prefix("gitdir:").map(str::trim).ok_or_else(|| {
        anyhow!(
            "invalid .git file in {} (expected gitdir:)",
            worktree.display()
        )
    })?;
    let p = Path::new(rest);
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        worktree.join(p)
    })
}

/// Stage an embedded repository as a gitlink (mode 160000) in the index.
///
/// Reads the HEAD of the embedded repo to get the commit OID, and warns
/// (unless `--no-warn-embedded-repo` is set) that a bare `git add` of an
/// embedded repo is probably a mistake.
fn stage_gitlink(
    _odb: &Odb,
    index: &mut Index,
    repo: &Repository,
    rel_path: &str,
    abs_path: &Path,
    dry_run: bool,
    verbose: bool,
    no_warn_embedded_repo: bool,
) -> Result<()> {
    let git_dir = embedded_repository_git_dir(abs_path)?;
    let super_fmt = read_object_format_from_git_dir(&repo.git_dir);
    let embedded_fmt = read_object_format_from_git_dir(&git_dir);
    if super_fmt != embedded_fmt {
        bail!("cannot add a submodule of a different hash algorithm");
    }

    let embedded_head_path = git_dir.join("HEAD");
    let head_content = fs::read_to_string(&embedded_head_path)
        .with_context(|| format!("cannot read HEAD of embedded repo '{}'", rel_path))?;
    let head_trimmed = head_content.trim();
    if head_trimmed.is_empty() {
        return Err(EmbeddedRepoNoCommitError(rel_path.to_owned()).into());
    }

    // Prefer resolving `HEAD` with the submodule work tree bound (like `git -C <sub> rev-parse
    // HEAD`): `Repository::open(git_dir, Some(worktree))` matches separate-git-dir layout.
    let oid_from_opened_repo = || -> Result<ObjectId> {
        let sub = Repository::open(&git_dir, Some(abs_path))
            .with_context(|| format!("open embedded repo '{}'", rel_path))?;
        let head = resolve_head(&sub.git_dir)
            .with_context(|| format!("resolve HEAD in embedded repo '{}'", rel_path))?;
        head.oid()
            .copied()
            .ok_or_else(|| anyhow!("unborn or invalid HEAD in embedded repo '{}'", rel_path))
    };

    let oid = match oid_from_opened_repo() {
        Ok(o) => o,
        Err(_) => {
            if head_trimmed.starts_with("ref: ") {
                match refs::resolve_ref(&git_dir, "HEAD") {
                    Ok(o) => o,
                    Err(_) => {
                        let mut found = None;
                        for branch in ["main", "master"] {
                            let p = git_dir.join("refs/heads").join(branch);
                            if let Ok(s) = fs::read_to_string(&p) {
                                if let Ok(o) = ObjectId::from_hex(s.trim()) {
                                    found = Some(o);
                                    break;
                                }
                            }
                        }
                        found.ok_or_else(|| EmbeddedRepoNoCommitError(rel_path.to_owned()))?
                    }
                }
            } else {
                ObjectId::from_hex(head_trimmed)
                    .with_context(|| format!("invalid HEAD OID in embedded repo '{}'", rel_path))?
            }
        }
    };

    // Check whether this entry is already tracked as a gitlink in the index
    let already_tracked = index
        .get(rel_path.as_bytes(), 0)
        .map(|e| e.mode == 0o160000)
        .unwrap_or(false);

    // Warn about embedded repository unless suppressed (Git prints the full hint block once).
    if !no_warn_embedded_repo && !already_tracked {
        eprintln!("warning: adding embedded git repository: {}", rel_path);
        if !EMBEDDED_REPO_FULL_HINT_EMITTED.get() {
            EMBEDDED_REPO_FULL_HINT_EMITTED.set(true);
            eprintln!("hint: You've added another git repository inside your current repository.");
            eprintln!("hint: Clones of the outer repository will not contain the contents of");
            eprintln!("hint: the embedded repository and will not know how to obtain it.");
            eprintln!("hint: If you meant to add a submodule, use:");
            eprintln!("hint:");
            eprintln!("hint: \tgit submodule add <url> {}", rel_path);
            eprintln!("hint:");
            eprintln!("hint: If you added this path by mistake, you can remove it from the");
            eprintln!("hint: index with:");
            eprintln!("hint:");
            eprintln!("hint: \tgit rm --cached {}", rel_path);
            eprintln!("hint:");
            eprintln!("hint: See \"git help submodule\" for more information.");
            eprintln!(
                "hint: Disable this message with \"git config set advice.addEmbeddedRepo false\""
            );
        }
    }

    if dry_run {
        dry_run_stdout_push_line(format!("add '{rel_path}'"));
        return Ok(());
    }

    remove_obstructing_parent_file_entries(index, rel_path);
    index.remove_descendants_under_path(rel_path);

    let meta = fs::metadata(abs_path)?;
    let entry = IndexEntry {
        ctime_sec: meta.ctime() as u32,
        ctime_nsec: meta.ctime_nsec() as u32,
        mtime_sec: meta.mtime() as u32,
        mtime_nsec: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        mode: 0o160000, // gitlink mode
        uid: meta.uid(),
        gid: meta.gid(),
        size: 0,
        oid,
        flags: rel_path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: rel_path.as_bytes().to_vec(),
        base_index_pos: 0,
    };
    index.stage_file(entry);

    if verbose {
        println!("add '{rel_path}'");
    }

    Ok(())
}

fn path_is_symlink(abs_path: &Path) -> bool {
    fs::symlink_metadata(abs_path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// True when `abs_path/.git` is a **nested** repository, not this repo's own `.git` at the work tree root.
fn is_nested_embedded_git_repo(abs_path: &Path, repo: &Repository) -> bool {
    let embedded = abs_path.join(".git");
    if !embedded.exists() {
        return false;
    }
    let Ok(emb) = fs::canonicalize(&embedded) else {
        return true;
    };
    let Ok(super_git) = fs::canonicalize(&repo.git_dir) else {
        return true;
    };
    emb != super_git
}

fn path_has_symlink_parent_for_add(work_tree: &Path, abs_path: &Path) -> bool {
    let Ok(rel) = abs_path.strip_prefix(work_tree) else {
        return false;
    };
    let mut cur = work_tree.to_path_buf();
    let mut comps = rel.components().peekable();
    while let Some(component) = comps.next() {
        if comps.peek().is_none() {
            break;
        }
        cur.push(component.as_os_str());
        if fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn remove_obstructing_parent_file_entries(index: &mut Index, rel_path: &str) {
    for (i, ch) in rel_path.char_indices() {
        if ch != '/' {
            continue;
        }
        let prefix = &rel_path[..i];
        let prefix_bytes = prefix.as_bytes();
        if let Some(e) = index.get(prefix_bytes, 0) {
            let is_tree = e.mode & 0o170000 == 0o040000;
            if !is_tree {
                index.remove(prefix_bytes);
            }
        }
    }
}

/// Stage a single file into the index.
pub(crate) fn stage_file(
    odb: &Odb,
    index: &mut Index,
    _work_tree: &Path,
    rel_path: &str,
    abs_path: &Path,
    repo: &Repository,
    ctx: &StageFileContext<'_>,
    add_cfg: &AddConfig,
) -> Result<()> {
    let (preserve_skip_worktree, sparse_blocked) = match index.get(rel_path.as_bytes(), 0) {
        Some(e) => (
            e.skip_worktree(),
            add_cfg
                .sparse
                .add_update_blocked(add_cfg.include_sparse, Some(e), rel_path),
        ),
        None => (
            false,
            add_cfg
                .sparse
                .add_update_blocked(add_cfg.include_sparse, None, rel_path),
        ),
    };
    if sparse_blocked {
        // Tracked entries: skip updating (Git `update_callback`); new paths: refuse (t3705).
        if index.get(rel_path.as_bytes(), 0).is_some() {
            return Ok(());
        }
        return Err(anyhow::Error::from(AddOutsideSparse {
            path: rel_path.to_string(),
        }));
    }

    if ctx.dry_run {
        if let Some(chmod_val) = ctx.chmod {
            let meta = fs::symlink_metadata(abs_path)?;
            if meta.file_type().is_symlink() {
                eprintln!("warning: cannot chmod {} '{}'", chmod_val, rel_path);
                return Err(anyhow::anyhow!("cannot chmod {} '{}'", chmod_val, rel_path));
            }
            return Ok(());
        }
        dry_run_stdout_push_line(format!("add '{rel_path}'"));
        return Ok(());
    }

    remove_obstructing_parent_file_entries(index, rel_path);
    index.remove_descendants_under_path(rel_path);

    if rel_path.ends_with(".gitattributes") && !path_is_symlink(abs_path) {
        let content = fs::read_to_string(abs_path).unwrap_or_default();
        let parsed = parse_gitattributes_file_content(&content, rel_path);
        if let Err(msg) = validate_rules_for_add(&parsed.rules, rel_path) {
            eprintln!("{msg}");
            // Do not stage invalid gitattributes; match Git behavior (exit 0, message on stderr).
            return Ok(());
        }
    }

    let meta = fs::symlink_metadata(abs_path)?;

    // Submodule / embedded repo roots appear as directories with `.git`; `walk_directory` records
    // the directory path without recursing, so we must stage them as gitlinks here.
    if meta.is_dir()
        && !meta.file_type().is_symlink()
        && is_nested_embedded_git_repo(abs_path, repo)
    {
        if repo
            .work_tree
            .as_deref()
            .is_some_and(|wt| submodule_ignore_all_for_add(repo, wt, rel_path))
            && index
                .get(rel_path.as_bytes(), 0)
                .is_some_and(|e| e.mode == 0o160000)
        {
            return Ok(());
        }
        return stage_gitlink(
            odb,
            index,
            repo,
            rel_path,
            abs_path,
            ctx.dry_run,
            ctx.verbose,
            false,
        );
    }

    if ctx.intent_to_add {
        // Don't clobber existing entries — only add the intent marker if not already staged
        if index.get(rel_path.as_bytes(), 0).is_some() {
            return Ok(());
        }
        let mode = if meta.file_type().is_symlink() {
            0o120000
        } else if add_cfg.core_filemode {
            normalize_mode(meta.mode())
        } else {
            0o100644 // When core.filemode=false, default to regular
        };
        let empty_oid = odb
            .write(ObjectKind::Blob, b"")
            .with_context(|| format!("writing empty blob for intent-to-add '{rel_path}'"))?;
        let mut entry = IndexEntry {
            ctime_sec: meta.ctime() as u32,
            ctime_nsec: meta.ctime_nsec() as u32,
            mtime_sec: meta.mtime() as u32,
            mtime_nsec: meta.mtime_nsec() as u32,
            dev: meta.dev() as u32,
            ino: meta.ino() as u32,
            mode,
            uid: meta.uid(),
            gid: meta.gid(),
            size: 0,
            oid: empty_oid,
            flags: rel_path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: rel_path.as_bytes().to_vec(),
            base_index_pos: 0,
        };
        entry.set_intent_to_add(true);
        index.add_or_replace(entry);
        if ctx.verbose {
            dry_run_stdout_push_line(format!("add '{rel_path}'"));
        }
        return Ok(());
    }

    // Determine mode
    let is_symlink = meta.file_type().is_symlink();
    let mode = if is_symlink {
        0o120000
    } else if add_cfg.core_filemode {
        normalize_mode(meta.mode())
    } else {
        // core.filemode=false: preserve existing mode from index if any,
        // otherwise default to 100644
        // Check for unmerged entries: prefer higher stages for mode
        let existing_mode = index
            .get(rel_path.as_bytes(), 0)
            .or_else(|| index.get(rel_path.as_bytes(), 2))
            .or_else(|| index.get(rel_path.as_bytes(), 1))
            .map(|e| e.mode);
        existing_mode.unwrap_or(0o100644)
    };

    // Handle --chmod flag
    let final_mode = if let Some(chmod_val) = ctx.chmod {
        if is_symlink {
            let display_path = rel_path;
            eprintln!("warning: cannot chmod {} '{}'", chmod_val, display_path);
            return Err(anyhow::anyhow!(
                "cannot chmod {} '{}'",
                chmod_val,
                display_path
            ));
        }
        match chmod_val {
            "+x" => 0o100755,
            "-x" => 0o100644,
            other => bail!("unrecognized --chmod value: {}", other),
        }
    } else {
        mode
    };

    // Do not skip based on stat cache alone: mtime/ctime can match across different contents
    // (common in tests and on fast filesystems), which would leave the index stale (t7601).
    // Also do not skip based on stat alone: two different blobs can share size/mtime (e.g. "0\n"
    // vs "1\n"), which breaks `git add -u` after small single-digit edits (t3415-rebase-autosquash).

    // Read file content and hash it
    let data = if is_symlink {
        let target = fs::read_link(abs_path)?;
        target.to_string_lossy().into_owned().into_bytes()
    } else {
        let raw = fs::read(abs_path)?;
        // Apply CRLF / clean-filter conversion (includes `working-tree-encoding` via
        // [`crlf::convert_to_git_with_opts`], matching Git `convert_to_git`).
        let file_attrs = crlf::get_file_attrs(&add_cfg.attrs, rel_path, false, &add_cfg.config);
        let prior_blob: Option<Vec<u8>> = index
            .get(rel_path.as_bytes(), 0)
            .filter(|e| e.oid != ObjectId::zero())
            .and_then(|e| odb.read(&e.oid).ok())
            .map(|o| o.data);
        let opts = crlf::ConvertToGitOpts {
            index_blob: prior_blob.as_deref(),
            renormalize: false,
            check_safecrlf: true,
        };
        match crlf::convert_to_git_with_opts(&raw, rel_path, &add_cfg.conv, &file_attrs, opts) {
            Ok(converted) => converted,
            Err(msg) => bail!("{msg}"),
        }
    };

    let oid = write_staged_blob(odb, repo, &data, add_cfg)?;
    let mut entry = entry_from_metadata(&meta, rel_path.as_bytes(), oid, final_mode);
    entry.mode = final_mode; // Ensure mode override sticks
    entry.set_assume_unchanged(false);
    entry.set_skip_worktree(preserve_skip_worktree);
    // Use stage_file which also clears conflict stages (1, 2, 3) for the same
    // path — this is how `git add` resolves merge/cherry-pick conflicts.
    index.stage_file(entry);
    if index.fsmonitor_last_update.is_some() {
        if let Some(staged) = index.get_mut(rel_path.as_bytes(), 0) {
            staged.set_fsmonitor_valid(true);
        }
    }

    if ctx.verbose {
        dry_run_stdout_push_line(format!("add '{rel_path}'"));
    }

    Ok(())
}

fn is_unwritable_odb_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            if io_err.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
        }
        if let Some(grit_err) = cause.downcast_ref::<grit_lib::error::Error>() {
            if let grit_lib::error::Error::Io(io_err) = grit_err {
                if io_err.kind() == std::io::ErrorKind::PermissionDenied {
                    return true;
                }
            }
        }
    }
    err.to_string().contains("Permission denied")
}

fn is_unwritable_lock_error(err: &grit_lib::error::Error) -> bool {
    matches!(
        err,
        grit_lib::error::Error::Io(io_err) if io_err.kind() == std::io::ErrorKind::AlreadyExists
    )
}

fn write_index_or_lock_err(repo: &Repository, index: &mut Index, index_path: &Path) -> Result<()> {
    repo.write_index_at(index_path, index).map_err(|e| {
        if is_unwritable_lock_error(&e) {
            let mut msg = format!(
                "Unable to create '{}': File exists.",
                index_path.with_extension("lock").display()
            );

            if let Some(pid_msg) = lockfile_pid_diagnostic(index_path) {
                msg.push_str("\n\n");
                msg.push_str(&pid_msg);
            } else {
                msg.push_str("\n\n");
                msg.push_str(
                    "Another git process seems to be running in this repository, or the lock file may be stale",
                );
            }

            anyhow!(msg)
        } else {
            anyhow!(e)
        }
    })
}

fn lockfile_pid_diagnostic(index_path: &Path) -> Option<String> {
    let pid_path = index_path.with_file_name("index~pid.lock");
    let pid_text = fs::read_to_string(&pid_path).ok()?;
    let pid = parse_pid_file(&pid_text)?;

    if is_pid_running(pid) {
        Some(format!(
            "Lock may be held by process {pid}; if no git process is running, the lock file may be stale (PIDs can be reused)"
        ))
    } else {
        Some(format!(
            "Lock was held by process {pid}, which is no longer running; the lock file appears to be stale"
        ))
    }
}

fn parse_pid_file(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let pid_str = trimmed.strip_prefix("pid ")?;
    pid_str.trim().parse::<u32>().ok()
}

fn is_pid_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let proc_path = Path::new("/proc").join(pid.to_string());
        proc_path.exists()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Collect `(index path, absolute path)` pairs under `dir` for staging.
pub(crate) fn collect_paths_for_stage_from_directory(
    dir: &Path,
    work_tree: &Path,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
    force: bool,
    precompose_unicode: bool,
) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    walk_directory(
        dir,
        work_tree,
        &mut out,
        repo,
        ignore_matcher,
        force,
        precompose_unicode,
    )?;
    Ok(out)
}

/// Recursively walk a directory, collecting index-relative paths and their on-disk paths.
fn walk_directory(
    dir: &Path,
    work_tree: &Path,
    out: &mut Vec<(String, PathBuf)>,
    repo: &Repository,
    ignore_matcher: &mut Option<IgnoreMatcher>,
    force: bool,
    precompose_unicode: bool,
) -> Result<()> {
    let entries = fs::read_dir(dir)?;
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let path = entry.path();
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        if name_str == ".git" {
            continue;
        }

        let rel_fs = path
            .strip_prefix(work_tree)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        let rel_index = if precompose_unicode {
            grit_lib::unicode_normalization::precompose_utf8_path(&rel_fs).into_owned()
        } else {
            rel_fs.clone()
        };

        // Use symlink_metadata to detect symlinks *before* following them.
        // A symlink to a directory should be stored as a symlink blob,
        // not traversed into.
        let ft = match fs::symlink_metadata(&path) {
            Ok(m) => m.file_type(),
            Err(_) => continue,
        };
        let is_symlink = ft.is_symlink();
        let is_dir = !is_symlink && ft.is_dir();

        // Check if ignored
        if !force {
            if let Some(matcher) = ignore_matcher.as_mut() {
                if let Ok((ignored, _)) = matcher.check_path(repo, None, &rel_index, is_dir) {
                    if ignored {
                        continue;
                    }
                }
            }
        }

        if is_dir {
            // Nested repository (submodule or embedded repo): record the directory itself so
            // `git add .` does not treat an existing gitlink as deleted (the inner `.git` is
            // skipped below and would otherwise hide the whole tree from the scan).
            if path.join(".git").exists() {
                out.push((rel_index, path));
                continue;
            }
            walk_directory(
                &path,
                work_tree,
                out,
                repo,
                ignore_matcher,
                force,
                precompose_unicode,
            )?;
        } else {
            out.push((rel_index, path));
        }
    }

    Ok(())
}

fn submodule_ignore_all_for_add(repo: &Repository, work_tree: &Path, rel: &str) -> bool {
    if let Ok(modules) =
        crate::commands::submodule::parse_gitmodules_with_repo(work_tree, Some(repo))
    {
        if let Some(module) = modules.iter().find(|m| m.path == rel) {
            let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
            let config_key = format!("submodule.{}.ignore", module.name);
            if config
                .get(&config_key)
                .is_some_and(|v| v.eq_ignore_ascii_case("all"))
            {
                return true;
            }
            return module
                .ignore
                .as_deref()
                .is_some_and(|v| v.eq_ignore_ascii_case("all"));
        }
    }
    false
}

/// Exit when running inside an unpopulated submodule worktree path.
///
/// Git discovers the superproject when invoked from an unpopulated submodule
/// directory. In that case, commands like `git -C sub add .` must fail with an
/// "in unpopulated submodule" fatal message instead of silently operating on
/// the superproject index.
fn die_if_in_unpopulated_submodule(index: &Index, prefix: Option<&str>) {
    let Some(prefix) = prefix else {
        return;
    };
    if prefix.is_empty() {
        return;
    }

    let prefix_bytes = prefix.as_bytes();
    for entry in &index.entries {
        if entry.mode != 0o160000 {
            continue;
        }
        let ce = entry.path.as_slice();
        let is_exact = prefix_bytes == ce;
        let is_inside = prefix_bytes.len() > ce.len()
            && prefix_bytes.starts_with(ce)
            && prefix_bytes[ce.len()] == b'/';
        if is_exact || is_inside {
            eprintln!(
                "fatal: in unpopulated submodule '{}'",
                String::from_utf8_lossy(ce)
            );
            std::process::exit(128);
        }
    }
}

/// Walk the parent components of `rel_path` (relative to `work_tree`) and
/// return `Some(prefix)` if any of them is a symbolic link.  Only *parent*
/// components are checked — the final path component itself may be a symlink.
fn check_symlink_in_path(work_tree: &Path, rel_path: &Path) -> Option<PathBuf> {
    let mut accumulated = PathBuf::new();
    let components: Vec<_> = rel_path.components().collect();
    // Check all components except the last one (the file itself).
    for component in components.iter().take(components.len().saturating_sub(1)) {
        accumulated.push(component);
        let abs = work_tree.join(&accumulated);
        if let Ok(meta) = fs::symlink_metadata(&abs) {
            if meta.file_type().is_symlink() {
                return Some(accumulated);
            }
        }
    }
    None
}

/// Expand a pathspec containing glob characters against the working tree.
///
/// If the pathspec does not contain glob characters, returns it unchanged.
/// Otherwise, matches it against files/dirs in the working tree directory.
pub(crate) fn expand_glob_pathspec(
    pathspec: &str,
    work_tree: &Path,
    precompose_unicode: bool,
) -> Vec<String> {
    if !grit_lib::pathspec::has_glob_chars(pathspec) {
        return vec![pathspec.to_owned()];
    }

    // Split into directory prefix and glob pattern.
    // e.g. "dir/file?.t" -> dir_prefix="dir", pattern="file?.t"
    let (dir_prefix, pattern) = if let Some(slash_pos) = pathspec.rfind('/') {
        (&pathspec[..slash_pos], &pathspec[slash_pos + 1..])
    } else {
        ("", pathspec)
    };

    let search_dir = if dir_prefix.is_empty() {
        work_tree.to_owned()
    } else {
        work_tree.join(dir_prefix)
    };

    let mut matches = Vec::new();
    if let Ok(entries) = fs::read_dir(&search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let raw = name.to_string_lossy();
            let raw_owned = raw.into_owned();
            let name_for_match = if precompose_unicode {
                precompose_utf8_segment(raw_owned.as_ref()).into_owned()
            } else {
                raw_owned.clone()
            };
            if name_for_match == ".git" {
                continue;
            }
            if wildmatch(pattern.as_bytes(), name_for_match.as_bytes(), 0) {
                // Use the filesystem spelling for `rel` so `open()` works on Linux when the index
                // stores NFC but `readdir` returns NFD (t3910 `git add *` on long filenames).
                let fs_name = raw_owned;
                let rel = if dir_prefix.is_empty() {
                    fs_name
                } else {
                    format!("{dir_prefix}/{fs_name}")
                };
                matches.push(rel);
            }
        }
    }

    // Git pathspec: a bracket pattern like `[abc]` matches one character class member *and*
    // the literal filename `[abc]` when present (wildmatch alone does not match that literal).
    if pattern.contains('[') && fs::symlink_metadata(search_dir.join(pattern)).is_ok() {
        let rel = if dir_prefix.is_empty() {
            pattern.to_string()
        } else {
            format!("{dir_prefix}/{pattern}")
        };
        if !matches.contains(&rel) {
            matches.push(rel);
        }
    }

    if matches.is_empty() {
        // No matches — return original pathspec so add_path gives a proper error.
        vec![pathspec.to_owned()]
    } else {
        matches.sort();
        matches
    }
}

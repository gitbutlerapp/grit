//! `grit checkout-index` — check out files from the index into the working tree.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::crlf;
use grit_lib::index::Index;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use std::io::{self, BufRead};
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::PathBuf;

use grit_lib::index::{IndexEntry, MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};
use grit_lib::repo::Repository;

/// Stage selection for `--stage` (last occurrence wins).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageMode {
    /// Default: stage 0 (merged entry).
    Normal,
    /// Unmerged stage 1, 2, or 3.
    One(u8),
    /// All unmerged stages (1–3) at once.
    All,
}

fn parse_stage_flag(s: &str) -> Result<StageMode, String> {
    match s {
        "all" => Ok(StageMode::All),
        "1" => Ok(StageMode::One(1)),
        "2" => Ok(StageMode::One(2)),
        "3" => Ok(StageMode::One(3)),
        _ => Err("stage should be between 1 and 3 or all".to_string()),
    }
}

fn effective_stage(stages: &[StageMode]) -> StageMode {
    stages.last().copied().unwrap_or(StageMode::Normal)
}

fn target_stage_for_single(mode: StageMode) -> u8 {
    match mode {
        StageMode::Normal => 0,
        StageMode::One(n) => n,
        StageMode::All => 0,
    }
}

/// Arguments for `grit checkout-index`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Checkout all files.
    #[arg(short = 'a', long = "all")]
    pub all: bool,

    /// Force overwrite existing files.
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Update stat info in the index.
    #[arg(short = 'u')]
    pub update_stat: bool,

    /// Be quiet.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Don't actually check out files.
    #[arg(short = 'n', long = "no-create")]
    pub dry_run: bool,

    /// Create leading directories.
    #[arg(long = "mkdir")]
    pub mkdir: bool,

    /// Read paths from stdin (NUL terminated if -z).
    #[arg(long)]
    pub stdin: bool,

    /// \0 line termination for --stdin.
    #[arg(short = 'z')]
    pub null_terminated: bool,

    /// Prefix to prepend to all checked-out paths.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Write to temp files instead of actual paths.
    #[arg(long = "temp", action = clap::ArgAction::SetTrue)]
    pub temp_explicit: bool,

    /// Do not write to temp files (cannot be combined with `--stage=all`).
    #[arg(long = "no-temp", action = clap::ArgAction::SetTrue)]
    pub no_temp: bool,

    /// Directory for temporary files (used with --temp).
    #[arg(long = "tmpdir", value_name = "dir")]
    pub tmpdir: Option<PathBuf>,

    /// Stage to check out (1, 2, 3, or all).
    #[arg(
        long = "stage",
        value_name = "STAGE",
        value_parser = parse_stage_flag,
        action = clap::ArgAction::Append
    )]
    pub stage: Vec<StageMode>,

    /// Ignore skip-worktree bits and checkout all entries.
    #[arg(long = "ignore-skip-worktree-bits")]
    pub ignore_skip_worktree_bits: bool,

    /// Files to check out (if not --all or --stdin).
    pub files: Vec<PathBuf>,
}

fn compute_use_temp(args: &Args, stage_mode: StageMode) -> Result<bool> {
    if args.no_temp {
        if stage_mode == StageMode::All {
            bail!("options '--stage=all' and '--no-temp' cannot be used together");
        }
        return Ok(false);
    }
    if args.temp_explicit {
        return Ok(true);
    }
    Ok(stage_mode == StageMode::All)
}

/// Run `grit checkout-index`.
pub fn run(args: Args) -> Result<()> {
    let stage_mode = effective_stage(&args.stage);
    let use_temp = compute_use_temp(&args, stage_mode)?;
    if args.tmpdir.is_some() && !use_temp {
        bail!("--tmpdir requires --temp");
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cannot checkout-index in bare repository"))?
        .to_path_buf();

    let index_path = repo.index_path();
    let mut index = repo.load_index_at(&index_path).context("loading index")?;

    // `load_index_at` transparently expands sparse-directory placeholders, losing the
    // collapsed-directory distinction git relies on to print `is a sparse directory`.
    // Parse the raw on-disk index to recover the set of sparse-directory prefixes.
    let sparse_dir_prefixes: Vec<Vec<u8>> = std::fs::read(&index_path)
        .ok()
        .and_then(|bytes| Index::parse(&bytes).ok())
        .map(|raw| {
            raw.entries
                .iter()
                .filter(|e| e.stage() == 0 && e.is_sparse_directory_placeholder())
                .map(|e| e.path.clone())
                .collect()
        })
        .unwrap_or_default();

    let cwd = repo.effective_pathspec_cwd();

    let prefix = args.prefix.as_deref().unwrap_or("");
    let symlinks_enabled = core_symlinks_enabled(&repo);
    let cwd_prefix = worktree_relative_prefix_bytes(&work_tree, &cwd)?;

    if stage_mode == StageMode::All {
        return run_checkout_stage_all(
            &repo,
            &mut index,
            &work_tree,
            &cwd,
            &cwd_prefix,
            prefix,
            symlinks_enabled,
            &args,
            use_temp,
        );
    }

    let target_stage = target_stage_for_single(stage_mode);

    let mut selected: Vec<(Vec<u8>, String)> = Vec::new();
    let mut index_needs_write = false;
    let mut had_error = false;
    if args.all {
        for entry in &index.entries {
            if entry.stage() != target_stage {
                continue;
            }
            if entry.skip_worktree() && !args.ignore_skip_worktree_bits {
                continue;
            }
            let disp = display_path_for_index_entry(&entry.path, &cwd_prefix);
            selected.push((entry.path.clone(), disp));
        }
    } else {
        // Collect requested (path_bytes, display) pairs from stdin or argv.
        let requested: Vec<(Vec<u8>, String)> = if args.stdin {
            let paths = read_stdin_paths(args.null_terminated)?;
            let mut out = Vec::new();
            for input_path in paths {
                let repo_path = resolve_repo_path(&work_tree, &cwd, &input_path)?;
                out.push((path_to_bytes(&repo_path), input_path.display().to_string()));
            }
            out
        } else {
            let mut out = Vec::new();
            for input_path in &args.files {
                let repo_path = resolve_repo_path(&work_tree, &cwd, input_path)?;
                out.push((path_to_bytes(&repo_path), input_path.display().to_string()));
            }
            out
        };

        for (path_bytes, display) in requested {
            // Git `index_name_pos` expands the sparse index on lookup; mirror that by
            // resolving the path against sparse-directory placeholders. The classification
            // (has_same_name / is_file / is_skipped) reproduces git's checkout_file().
            let class = classify_checkout_index_path(
                &index,
                &sparse_dir_prefixes,
                &path_bytes,
                target_stage,
            );
            match class {
                PathClass::File { skip_worktree } => {
                    if skip_worktree && !args.ignore_skip_worktree_bits {
                        if !args.quiet {
                            eprintln!("git checkout-index: {display} has skip-worktree enabled; use '--ignore-skip-worktree-bits' to checkout");
                        }
                        had_error = true;
                        continue;
                    }
                    selected.push((path_bytes, display));
                }
                PathClass::SparseDirectory => {
                    if !args.quiet {
                        eprintln!("git checkout-index: {display} is a sparse directory");
                    }
                    had_error = true;
                }
                PathClass::NotInCache => {
                    if !args.quiet {
                        eprintln!("git checkout-index: {display} is not in the cache");
                    }
                    had_error = true;
                }
            }
        }
    }

    let mut has_errors = had_error;
    for (path, display) in selected {
        let entry = index
            .get(&path, target_stage)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("'{}' is not in the cache", display))?;
        match checkout_entry(
            &repo,
            &mut index,
            &entry,
            &work_tree,
            prefix,
            symlinks_enabled,
            &args,
            use_temp,
            &display,
        ) {
            Ok(outcome) => {
                if let Some(updated) = outcome.updated_entry {
                    index.add_or_replace(updated);
                    index_needs_write = true;
                }
                if let Some(line) = outcome.temp_output {
                    println!("{line}");
                }
            }
            Err(_) => {
                has_errors = true;
            }
        }
    }

    if index_needs_write {
        repo.write_index_at(&index_path, &mut index)
            .context("writing index")?;
    }

    if has_errors {
        std::process::exit(1);
    }

    Ok(())
}

/// Classification of a requested checkout-index path, mirroring git's `checkout_file`
/// (builtin/checkout-index.c) which inspects `has_same_name`, `is_file`, `is_skipped`.
enum PathClass {
    /// A regular blob entry exists at this exact path.
    File { skip_worktree: bool },
    /// The path resolves to a sparse-directory placeholder (a collapsed out-of-cone dir).
    SparseDirectory,
    /// No entry with this name exists in the index.
    NotInCache,
}

fn classify_checkout_index_path(
    index: &Index,
    sparse_dir_prefixes: &[Vec<u8>],
    path: &[u8],
    stage: u8,
) -> PathClass {
    // A request for a path that the (collapsed) on-disk index represented as a
    // sparse-directory placeholder is reported as a sparse directory by git, even though
    // grit has already expanded that placeholder in memory.
    for pref in sparse_dir_prefixes {
        let without_slash = pref.strip_suffix(b"/").unwrap_or(pref);
        if path == pref.as_slice() || path == without_slash {
            return PathClass::SparseDirectory;
        }
    }
    if let Some(e) = index.get(path, stage) {
        return PathClass::File {
            skip_worktree: e.skip_worktree(),
        };
    }
    PathClass::NotInCache
}

#[expect(clippy::too_many_arguments)]
fn run_checkout_stage_all(
    repo: &Repository,
    index: &mut Index,
    work_tree: &std::path::Path,
    cwd: &std::path::Path,
    cwd_prefix: &[u8],
    prefix: &str,
    symlinks_enabled: bool,
    args: &Args,
    use_temp: bool,
) -> Result<()> {
    if !use_temp {
        bail!("internal error: --stage=all requires temp");
    }
    if args.all {
        checkout_all_unmerged_stages(
            repo,
            index,
            work_tree,
            cwd_prefix,
            prefix,
            symlinks_enabled,
            args,
        )?;
        return Ok(());
    }

    if args.stdin {
        let paths = read_stdin_paths(args.null_terminated)?;
        for input_path in paths {
            let repo_path = resolve_repo_path(work_tree, cwd, &input_path)?;
            let path_bytes = path_to_bytes(&repo_path);
            if let Some(line) = checkout_stages_one_path(
                repo,
                index,
                work_tree,
                prefix,
                symlinks_enabled,
                args,
                &path_bytes,
                &input_path.display().to_string(),
            )? {
                println!("{line}");
            }
        }
        return Ok(());
    }

    for input_path in &args.files {
        let repo_path = resolve_repo_path(work_tree, cwd, input_path)?;
        let path_bytes = path_to_bytes(&repo_path);
        if let Some(line) = checkout_stages_one_path(
            repo,
            index,
            work_tree,
            prefix,
            symlinks_enabled,
            args,
            &path_bytes,
            &input_path.display().to_string(),
        )? {
            println!("{line}");
        }
    }

    Ok(())
}

fn index_has_any_entry(index: &Index, path_bytes: &[u8]) -> bool {
    index.entries.iter().any(|e| e.path == path_bytes)
}

#[expect(clippy::too_many_arguments)]
fn checkout_stages_one_path(
    repo: &Repository,
    index: &Index,
    work_tree: &std::path::Path,
    prefix: &str,
    symlinks_enabled: bool,
    args: &Args,
    path_bytes: &[u8],
    display_path: &str,
) -> Result<Option<String>> {
    let mut top: [Option<PathBuf>; 4] = [None, None, None, None];
    let mut did = false;
    for stage in 1u8..=3u8 {
        if let Some(entry) = index.get(path_bytes, stage) {
            did = true;
            let line = checkout_one_stage_to_temp_line(
                repo,
                index,
                entry,
                work_tree,
                prefix,
                symlinks_enabled,
                args,
                stage,
            )?;
            top[stage as usize] = Some(line);
        }
    }
    if did {
        return Ok(Some(format_stage_all_line(&top, display_path, work_tree)));
    }
    if index_has_any_entry(index, path_bytes) {
        return Ok(None);
    }
    // Match Git: `git checkout-index: <path> is not in the cache`
    eprintln!("grit checkout-index: {display_path} is not in the cache");
    Err(anyhow::anyhow!("'{display_path}' is not in the cache"))
}

#[expect(clippy::too_many_arguments)]
fn checkout_entry(
    repo: &Repository,
    index: &mut Index,
    entry: &grit_lib::index::IndexEntry,
    work_tree: &std::path::Path,
    prefix: &str,
    symlinks_enabled: bool,
    args: &Args,
    use_temp: bool,
    display_path: &str,
) -> Result<CheckoutOutcome> {
    let path_str = String::from_utf8_lossy(&entry.path).into_owned();
    let rel_path = format!("{prefix}{path_str}");
    let abs_path = work_tree.join(&rel_path);
    let mut outcome = CheckoutOutcome::default();

    if args.dry_run {
        return Ok(outcome);
    }

    if entry.mode == 0o160000 {
        if use_temp {
            eprintln!("cannot create temporary submodule {path_str}");
            return Err(anyhow::anyhow!(
                "cannot create temporary submodule {path_str}"
            ));
        }
        return Ok(outcome);
    }

    let obj = match repo.odb.read(&entry.oid) {
        Ok(obj) => obj,
        Err(_) => {
            eprintln!("unable to read sha1 file of {path_str} ({})", entry.oid);
            return Err(anyhow::anyhow!(
                "unable to read sha1 file of {path_str} ({})",
                entry.oid
            ));
        }
    };
    if obj.kind != ObjectKind::Blob {
        bail!("cannot checkout non-blob at '{path_str}'");
    }

    if use_temp {
        let tmp_path = write_temp_blob(entry, &obj.data, args, work_tree)?;
        outcome.temp_output = Some(format!(
            "{}\t{display_path}",
            display_temp_path_for_stdout(&tmp_path, work_tree)
        ));
        return Ok(outcome);
    }

    let existing_meta = std::fs::symlink_metadata(&abs_path).ok();
    if let Some(ref meta) = existing_meta {
        // `--ignore-skip-worktree-bits` only permits checking out skip-worktree entries; it
        // does NOT bypass the force guard, so a changed file present on disk still errors.
        if !args.force {
            let unchanged = if entry.mode == MODE_SYMLINK && symlinks_enabled {
                meta.file_type().is_symlink()
                    && std::fs::read_link(&abs_path).ok().is_some_and(|t| {
                        use std::os::unix::ffi::OsStrExt;
                        t.as_os_str().as_bytes() == obj.data.as_slice()
                    })
            } else if !meta.file_type().is_symlink() && entry.mode != MODE_SYMLINK {
                let wt_mode = worktree_mode_from_metadata(meta);
                if wt_mode != entry.mode {
                    false
                } else {
                    worktree_clean_blob_oid_matches(
                        repo, index, work_tree, &path_str, &abs_path, &entry.oid,
                    )
                    .unwrap_or(false)
                }
            } else {
                false
            };
            if unchanged {
                return Ok(outcome);
            }
            if meta.is_dir() {
                if !args.quiet {
                    eprintln!("{rel_path} already exists, no checkout");
                }
                return Err(anyhow::anyhow!("{rel_path} already exists, no checkout"));
            }
            // Without --force, leave an existing changed path alone.
            if !args.quiet {
                eprintln!("{rel_path} already exists, no checkout");
            }
            return Err(anyhow::anyhow!("{rel_path} already exists, no checkout"));
        }
    }

    if let Some(parent) = abs_path.parent() {
        // Replace a symlink or file at the immediate parent path when we need a directory
        // (matches Git: e.g. path1 -> path2 symlink, or tmp-path1 file blocking tmp-path1/file1).
        // Preserve a leading symlink when it is the first path component of --prefix (e.g.
        // tmp -> tmp1 with --prefix=tmp/orary- so checkout goes under tmp1/orary-path*).
        let keep_symlink = should_keep_symlink_parent_for_prefix(parent, work_tree, prefix);
        if let Ok(meta) = std::fs::symlink_metadata(parent) {
            if !args.force && (meta.file_type().is_symlink() || meta.is_file()) && !keep_symlink {
                if !args.quiet {
                    eprintln!("{rel_path} already exists, no checkout");
                }
                return Err(anyhow::anyhow!("{rel_path} already exists, no checkout"));
            }
            if meta.file_type().is_symlink() {
                if !keep_symlink {
                    let _ = std::fs::remove_file(parent);
                }
            } else if meta.is_file() {
                let _ = std::fs::remove_file(parent);
            }
        }
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    if abs_path.is_dir() {
        std::fs::remove_dir_all(&abs_path)?;
    } else if existing_meta.is_some() {
        std::fs::remove_file(&abs_path)?;
    }

    if entry.mode == MODE_SYMLINK && symlinks_enabled {
        let target = String::from_utf8(obj.data)
            .map_err(|_| anyhow::anyhow!("symlink target is not UTF-8"))?;
        std::os::unix::fs::symlink(&target, &abs_path)?;
    } else {
        let data = {
            let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
            let conv = crlf::ConversionConfig::from_config(&config);
            let attrs =
                crlf::load_gitattributes_for_checkout(work_tree, &path_str, index, &repo.odb);
            let file_attrs = crlf::get_file_attrs(&attrs, &path_str, false, &config);
            let oid_hex = format!("{}", entry.oid);
            let smudge_meta = grit_lib::filter_process::smudge_meta_for_checkout(repo, &oid_hex);
            crlf::convert_to_worktree_eager(
                &obj.data,
                &path_str,
                &conv,
                &file_attrs,
                Some(&oid_hex),
                Some(&smudge_meta),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?
        };
        std::fs::write(&abs_path, &data)?;

        if entry.mode == MODE_EXECUTABLE {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&abs_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&abs_path, perms)?;
        }
    }

    if args.update_stat && prefix.is_empty() && entry.stage() == 0 {
        outcome.updated_entry = Some(refresh_stat_for_entry(entry, &abs_path)?);
    }

    Ok(outcome)
}

#[expect(clippy::too_many_arguments)]
fn checkout_one_stage_to_temp_line(
    repo: &Repository,
    _index: &Index,
    entry: &IndexEntry,
    work_tree: &std::path::Path,
    prefix: &str,
    symlinks_enabled: bool,
    args: &Args,
    stage: u8,
) -> Result<PathBuf> {
    let _ = (work_tree, prefix, symlinks_enabled, stage);
    if entry.mode == 0o160000 {
        bail!("cannot create temporary submodule");
    }
    let obj = repo.odb.read(&entry.oid)?;
    if obj.kind != ObjectKind::Blob {
        bail!("cannot checkout non-blob");
    }
    write_temp_blob(entry, &obj.data, args, work_tree)
}

fn display_temp_path_for_stdout(p: &std::path::Path, work_tree: &std::path::Path) -> String {
    if let Ok(r) = p.strip_prefix(work_tree) {
        let s = r.to_string_lossy().to_string();
        return s.strip_prefix("./").map(String::from).unwrap_or(s);
    }
    let s = p.display().to_string();
    s.strip_prefix("./").map(String::from).unwrap_or(s)
}

fn format_stage_all_line(
    top: &[Option<PathBuf>; 4],
    display_path: &str,
    work_tree: &std::path::Path,
) -> String {
    let mut s = String::new();
    for (i, slot) in top.iter().skip(1).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        match slot {
            Some(p) => s.push_str(&display_temp_path_for_stdout(p, work_tree)),
            None => s.push('.'),
        }
    }
    s.push('\t');
    s.push_str(display_path);
    s
}

fn checkout_all_unmerged_stages(
    repo: &Repository,
    index: &Index,
    work_tree: &std::path::Path,
    cwd_prefix: &[u8],
    prefix: &str,
    symlinks_enabled: bool,
    args: &Args,
) -> Result<()> {
    let mut i = 0usize;
    while i < index.entries.len() {
        let path = index.entries[i].path.clone();
        let start = i;
        while i < index.entries.len() && index.entries[i].path == path {
            i += 1;
        }
        let group = &index.entries[start..i];
        if !cwd_prefix.is_empty() && !path.starts_with(cwd_prefix) {
            continue;
        }
        let mut top: [Option<PathBuf>; 4] = [None, None, None, None];
        let mut any = false;
        for entry in group {
            let st = entry.stage();
            if st == 0 || st > 3 {
                continue;
            }
            if entry.skip_worktree() && !args.ignore_skip_worktree_bits {
                continue;
            }
            any = true;
            let p = checkout_one_stage_to_temp_line(
                repo,
                index,
                entry,
                work_tree,
                prefix,
                symlinks_enabled,
                args,
                st,
            )?;
            top[st as usize] = Some(p);
        }
        if any {
            let disp = display_path_for_index_entry(&path, cwd_prefix);
            println!("{}", format_stage_all_line(&top, &disp, work_tree));
        }
    }
    Ok(())
}

#[derive(Default)]
struct CheckoutOutcome {
    updated_entry: Option<grit_lib::index::IndexEntry>,
    temp_output: Option<String>,
}

fn worktree_mode_from_metadata(meta: &std::fs::Metadata) -> u32 {
    if meta.file_type().is_symlink() {
        MODE_SYMLINK
    } else if meta.mode() & 0o111 != 0 {
        MODE_EXECUTABLE
    } else {
        MODE_REGULAR
    }
}

/// When cached stat does not match the file (e.g. index not refreshed after `commit`), still treat
/// the path as up to date if the cleaned working-tree content hashes to the same blob as the index
/// (matches Git `ie_match_stat` / `hash_stat_data` behavior).
fn worktree_clean_blob_oid_matches(
    repo: &Repository,
    index: &Index,
    work_tree: &std::path::Path,
    path_str: &str,
    abs_path: &std::path::Path,
    index_oid: &ObjectId,
) -> Result<bool> {
    let raw = match std::fs::read(abs_path) {
        Ok(b) => b,
        Err(_) => return Ok(false),
    };
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let conv = crlf::ConversionConfig::from_config(&config);
    let attrs = crlf::load_gitattributes_for_checkout(work_tree, path_str, index, &repo.odb);
    let file_attrs = crlf::get_file_attrs(&attrs, path_str, false, &config);
    let cleaned = crlf::convert_to_git(&raw, path_str, &conv, &file_attrs).unwrap_or(raw);
    let wt_oid = Odb::hash_object_data(ObjectKind::Blob, &cleaned);
    Ok(wt_oid == *index_oid)
}

fn read_stdin_paths(null_terminated: bool) -> Result<Vec<PathBuf>> {
    let stdin = io::stdin();
    let mut paths = Vec::new();

    if null_terminated {
        use io::Read;
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf)?;
        for part in buf.split(|&b| b == 0) {
            if !part.is_empty() {
                let s = std::str::from_utf8(part).context("non-UTF-8 path")?;
                paths.push(PathBuf::from(s));
            }
        }
    } else {
        for line in stdin.lock().lines() {
            let line = line?;
            if !line.is_empty() {
                paths.push(PathBuf::from(line));
            }
        }
    }
    Ok(paths)
}

fn refresh_stat_for_entry(
    entry: &grit_lib::index::IndexEntry,
    abs_path: &std::path::Path,
) -> Result<grit_lib::index::IndexEntry> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(abs_path)
        .with_context(|| format!("cannot stat '{}'", abs_path.display()))?;
    let mut refreshed = entry.clone();
    refreshed.ctime_sec = meta.ctime() as u32;
    refreshed.ctime_nsec = meta.ctime_nsec() as u32;
    refreshed.mtime_sec = meta.mtime() as u32;
    refreshed.mtime_nsec = meta.mtime_nsec() as u32;
    refreshed.dev = meta.dev() as u32;
    refreshed.ino = meta.ino() as u32;
    refreshed.uid = meta.uid();
    refreshed.gid = meta.gid();
    refreshed.size = meta.size() as u32;
    Ok(refreshed)
}

fn write_temp_blob(
    entry: &grit_lib::index::IndexEntry,
    data: &[u8],
    args: &Args,
    work_tree: &std::path::Path,
) -> Result<PathBuf> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::time::{SystemTime, UNIX_EPOCH};

    let base_dir = args
        .tmpdir
        .as_ref()
        .cloned()
        .unwrap_or_else(|| work_tree.to_path_buf());
    if !base_dir.exists() {
        std::fs::create_dir_all(&base_dir).with_context(|| {
            format!(
                "cannot create tmpdir '{}'",
                base_dir.as_path().to_string_lossy()
            )
        })?;
    }

    let pid = std::process::id();
    for attempt in 0..1000u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!(".merge_file_{pid}_{nanos}_{attempt}");
        let candidate = base_dir.join(name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "cannot create temp file '{}': {err}",
                    candidate.display()
                ));
            }
        };

        file.write_all(data)
            .with_context(|| format!("cannot write temp file '{}'", candidate.display()))?;

        if entry.mode == MODE_EXECUTABLE {
            let mut perms = std::fs::metadata(&candidate)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&candidate, perms)?;
        }

        return Ok(candidate);
    }

    bail!(
        "unable to create unique temporary file in '{}'",
        base_dir.display()
    )
}

fn core_symlinks_enabled(repo: &Repository) -> bool {
    let config_path = repo.git_dir.join("config");
    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return true,
    };

    let mut in_core = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let section = line[1..line.len() - 1].trim().to_ascii_lowercase();
            in_core = section == "core";
            continue;
        }
        if !in_core {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("symlinks") {
            let v = value.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "false" | "no" | "off" | "0") {
                return false;
            }
            if matches!(v.as_str(), "true" | "yes" | "on" | "1") {
                return true;
            }
        }
    }
    true
}

fn should_keep_symlink_parent_for_prefix(
    parent: &std::path::Path,
    work_tree: &std::path::Path,
    prefix: &str,
) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let Ok(rel) = parent.strip_prefix(work_tree) else {
        return false;
    };
    let rel_s = rel.to_string_lossy();
    let rel_s = rel_s.as_ref();
    let p = prefix.trim_end_matches('/');
    if p.is_empty() {
        return false;
    }
    let first = p.split('/').next().unwrap_or("");
    if first.is_empty() {
        return false;
    }
    rel_s == first
}

fn worktree_relative_prefix_bytes(
    work_tree: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<Vec<u8>> {
    let wt = std::fs::canonicalize(work_tree).unwrap_or_else(|_| work_tree.to_path_buf());
    let cw = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let rel = cw.strip_prefix(&wt).with_context(|| {
        format!(
            "current directory '{}' is outside repository work tree '{}'",
            cwd.display(),
            work_tree.display()
        )
    })?;
    if rel.as_os_str().is_empty() {
        return Ok(Vec::new());
    }
    let mut s = rel.to_string_lossy().replace('\\', "/");
    if !s.ends_with('/') {
        s.push('/');
    }
    Ok(s.into_bytes())
}

fn display_path_for_index_entry(entry_path: &[u8], cwd_prefix: &[u8]) -> String {
    let full = String::from_utf8_lossy(entry_path);
    if cwd_prefix.is_empty() {
        return full.into_owned();
    }
    if entry_path.starts_with(cwd_prefix) {
        String::from_utf8_lossy(&entry_path[cwd_prefix.len()..]).into_owned()
    } else {
        full.into_owned()
    }
}

fn resolve_repo_path(
    work_tree: &std::path::Path,
    cwd: &std::path::Path,
    input: &std::path::Path,
) -> Result<PathBuf> {
    let combined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let normalized = normalize_path(&combined);
    let rel = normalized
        .strip_prefix(work_tree)
        .with_context(|| format!("path '{}' is outside repository work tree", input.display()))?;
    Ok(rel.to_path_buf())
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

fn path_to_bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

//! Git-compatible `difftool` engine.
//!
//! Launches external diff viewers for changed paths, mirroring Git's
//! `git-difftool` / `git-difftool--helper` behavior.

use crate::config::ConfigSet;
use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree, diff_tree_to_worktree, diff_trees, DiffEntry,
    DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::Index;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::repo::Repository;
use crate::rev_parse::{peel_to_tree, resolve_revision};
use crate::state::resolve_head;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Environment overrides mirroring Git's `GIT_*` difftool variables.
#[derive(Debug, Clone, Default)]
pub struct DifftoolEnv {
    /// `GIT_DIFF_TOOL` — force a particular tool name.
    pub git_diff_tool: Option<String>,
    /// `GIT_DIFFTOOL_NO_PROMPT` is set (any value).
    pub git_difftool_no_prompt: bool,
    /// `GIT_DIFFTOOL_PROMPT` is set (any value).
    pub git_difftool_prompt: bool,
    /// `GIT_MERGETOOL_GUI` — `"true"` / `"false"` when explicitly set.
    pub git_mergetool_gui: Option<bool>,
    /// `DISPLAY` for `difftool.guiDefault=auto`.
    pub display: Option<String>,
}

/// Parsed difftool-specific CLI flags (not forwarded to `diff`).
#[derive(Debug, Clone, Default)]
pub struct DifftoolOptions {
    /// `-g` / `--gui` when explicitly true.
    pub gui: Option<bool>,
    /// `-d` / `--dir-diff`.
    pub dir_diff: bool,
    /// `-y` / `--no-prompt` → false; `--prompt` → true; unset → use config/env.
    pub prompt: Option<bool>,
    /// `--trust-exit-code`.
    pub trust_exit_code: bool,
    /// `--no-trust-exit-code`.
    pub no_trust_exit_code: bool,
    /// `-t` / `--tool`.
    pub tool: Option<String>,
    /// `-x` / `--extcmd`.
    pub extcmd: Option<String>,
    /// `--tool-help`.
    pub tool_help: bool,
    /// `--no-index` (forwarded to diff, but also recorded here).
    pub no_index: bool,
    /// `--symlinks` / `--no-symlinks` for dir-diff.
    pub symlinks: Option<bool>,
    /// `--rotate-to=<path>`.
    pub rotate_to: Option<String>,
    /// `--skip-to=<path>`.
    pub skip_to: Option<String>,
    /// Remaining arguments forwarded to diff (revs, `--cached`, paths, …).
    pub diff_argv: Vec<String>,
}

/// Result of a difftool run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DifftoolResult {
    /// Process exit code (0 = success).
    pub exit_code: i32,
}

/// Parse `argv` into [`DifftoolOptions`], consuming only difftool-specific flags.
///
/// Unknown options and positional arguments are collected into `diff_argv`.
pub fn parse_difftool_argv(argv: &[String]) -> Result<DifftoolOptions> {
    let mut opts = DifftoolOptions::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        match arg.as_str() {
            "-g" | "--gui" => {
                opts.gui = Some(true);
            }
            "--no-gui" => {
                opts.gui = Some(false);
            }
            "-d" | "--dir-diff" => {
                opts.dir_diff = true;
            }
            "-y" | "--no-prompt" => {
                opts.prompt = Some(false);
            }
            "--prompt" => {
                opts.prompt = Some(true);
            }
            "--trust-exit-code" => {
                opts.trust_exit_code = true;
            }
            "--no-trust-exit-code" => {
                opts.no_trust_exit_code = true;
            }
            "--tool-help" => {
                opts.tool_help = true;
            }
            "--no-index" => {
                opts.no_index = true;
                opts.diff_argv.push(arg.clone());
            }
            "--symlinks" => {
                opts.symlinks = Some(true);
            }
            "--no-symlinks" => {
                opts.symlinks = Some(false);
            }
            "-t" | "--tool" => {
                i += 1;
                let val = argv
                    .get(i)
                    .ok_or_else(|| Error::Message("option '--tool' requires an argument".into()))?;
                opts.tool = Some(parse_tool_value(val)?);
            }
            "-x" | "--extcmd" => {
                i += 1;
                let val = argv.get(i).ok_or_else(|| {
                    Error::Message("option '--extcmd' requires an argument".into())
                })?;
                opts.extcmd = Some(val.clone());
            }
            s if s.starts_with("--tool=") => {
                opts.tool = Some(parse_tool_value(s.strip_prefix("--tool=").unwrap_or(""))?);
            }
            s if s.starts_with("--extcmd=") => {
                opts.extcmd = Some(s.strip_prefix("--extcmd=").unwrap_or("").to_string());
            }
            s if s.starts_with("--rotate-to=") => {
                opts.rotate_to = Some(s.strip_prefix("--rotate-to=").unwrap_or("").to_string());
            }
            s if s.starts_with("--skip-to=") => {
                opts.skip_to = Some(s.strip_prefix("--skip-to=").unwrap_or("").to_string());
            }
            "--" => {
                opts.diff_argv.push("--".to_string());
                opts.diff_argv.extend_from_slice(&argv[i + 1..]);
                break;
            }
            _ if arg.starts_with('-') => {
                opts.diff_argv.push(arg.clone());
            }
            _ => {
                opts.diff_argv.push(arg.clone());
            }
        }
        i += 1;
    }
    Ok(opts)
}

fn parse_tool_value(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Err(Error::Message("no <tool> given for --tool=<tool>".into()));
    }
    Ok(raw.to_string())
}

/// Print built-in / configured diff tools (like `git difftool --tool-help`).
pub fn print_tool_help(config: &ConfigSet, stdout: &mut dyn Write) -> io::Result<()> {
    writeln!(
        stdout,
        "'git difftool --tool=<tool>' may be set to one of the following:"
    )?;
    writeln!(stdout)?;
    let mut names = BTreeSet::new();
    for entry in config.entries() {
        if let Some(rest) = entry.key.strip_prefix("difftool.") {
            if let Some(tool) = rest.strip_suffix(".cmd") {
                names.insert(tool.to_string());
            }
        }
        if let Some(rest) = entry.key.strip_prefix("mergetool.") {
            if let Some(tool) = rest.strip_suffix(".cmd") {
                names.insert(tool.to_string());
            }
        }
    }
    for tool in &names {
        writeln!(stdout, "\t{tool:<15}")?;
    }
    for tool in ["vimdiff", "meld", "kompare", "tkdiff"] {
        if !names.contains(tool) {
            writeln!(stdout, "\t{tool:<15}")?;
        }
    }
    writeln!(stdout)?;
    Ok(())
}

/// Run difftool against `repo` (or without repo for `--no-index`).
pub fn run_difftool(
    repo: Option<&Repository>,
    opts: &DifftoolOptions,
    env: &DifftoolEnv,
    config: &ConfigSet,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
) -> Result<DifftoolResult> {
    if opts.tool_help {
        print_tool_help(config, stdout)?;
        return Ok(DifftoolResult { exit_code: 0 });
    }

    if opts.no_index {
        return run_no_index_difftool(opts, env, config, stdin, stdout);
    }

    let repo = repo.ok_or_else(|| Error::NotARepository(".".into()))?;
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    if opts.gui.is_some() && opts.tool.is_some() {
        return Err(Error::Message(
            "options '--gui' and '--tool' cannot be used together".into(),
        ));
    }
    if opts.gui.is_some() && opts.extcmd.is_some() {
        return Err(Error::Message(
            "options '--gui' and '--extcmd' cannot be used together".into(),
        ));
    }
    if opts.tool.is_some() && opts.extcmd.is_some() {
        return Err(Error::Message(
            "options '--tool' and '--extcmd' cannot be used together".into(),
        ));
    }

    let trust_exit_code = resolve_trust_exit_code(opts, config);
    let should_prompt = if opts.dir_diff {
        false
    } else {
        resolve_should_prompt(opts, env, config)
    };
    let tool_ctx = resolve_tool_context(opts, env, config)?;

    let index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };

    let mut entries = collect_diff_entries(repo, &index, work_tree, &opts.diff_argv)?;
    entries = apply_rotate_skip(entries, opts.rotate_to.as_deref(), opts.skip_to.as_deref())?;

    if entries.is_empty() {
        return Ok(DifftoolResult { exit_code: 0 });
    }

    if opts.dir_diff {
        return run_dir_diff(
            repo,
            &entries,
            work_tree,
            &index,
            &tool_ctx,
            opts,
            env,
            config,
            trust_exit_code,
            should_prompt,
            stdin,
            stdout,
        );
    }

    let tmp_dir = tempfile::tempdir().map_err(Error::Io)?;
    let total = entries.len();
    for (idx, entry) in entries.iter().enumerate() {
        let counter = idx + 1;
        let exit = launch_file_diff(
            repo,
            entry,
            work_tree,
            tmp_dir.path(),
            &tool_ctx,
            counter,
            total,
            should_prompt,
            trust_exit_code,
            stdin,
            stdout,
        )?;
        if exit != 0 && trust_exit_code {
            return Ok(DifftoolResult { exit_code: exit });
        }
        if exit >= 126 {
            return Ok(DifftoolResult { exit_code: exit });
        }
    }
    Ok(DifftoolResult { exit_code: 0 })
}

/// Tool resolution context for launching a diff viewer.
#[derive(Debug, Clone)]
struct ToolContext {
    tool_name: String,
    extcmd: Option<String>,
    tool_cmd: Option<String>,
    tool_path: Option<String>,
}

fn resolve_trust_exit_code(opts: &DifftoolOptions, config: &ConfigSet) -> bool {
    if opts.no_trust_exit_code {
        return false;
    }
    if opts.trust_exit_code {
        return true;
    }
    config
        .get_bool("difftool.trustExitCode")
        .and_then(|r| r.ok())
        .unwrap_or(false)
}

fn resolve_should_prompt(opts: &DifftoolOptions, env: &DifftoolEnv, config: &ConfigSet) -> bool {
    if env.git_difftool_no_prompt {
        return false;
    }
    if env.git_difftool_prompt {
        return true;
    }
    if let Some(p) = opts.prompt {
        return p;
    }
    let prompt_merge = config
        .get_bool("mergetool.prompt")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    config
        .get_bool("difftool.prompt")
        .and_then(|r| r.ok())
        .unwrap_or(prompt_merge)
}

fn gui_default(config: &ConfigSet, env: &DifftoolEnv) -> Result<bool> {
    let raw = config
        .get("difftool.guiDefault")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "false".to_string());
    if raw == "auto" {
        return Ok(env.display.as_ref().is_some_and(|d| !d.is_empty()));
    }
    Ok(config
        .get_bool("difftool.guiDefault")
        .and_then(|r| r.ok())
        .unwrap_or(false))
}

fn resolve_tool_context(
    opts: &DifftoolOptions,
    env: &DifftoolEnv,
    config: &ConfigSet,
) -> Result<ToolContext> {
    if let Some(ext) = &opts.extcmd {
        return Ok(ToolContext {
            tool_name: ext.clone(),
            extcmd: Some(ext.clone()),
            tool_cmd: None,
            tool_path: None,
        });
    }

    let use_gui = match opts.gui {
        Some(v) => v,
        None => match env.git_mergetool_gui {
            Some(v) => v,
            None => gui_default(config, env)?,
        },
    };

    let tool_name = if let Some(t) = opts.tool.clone().or_else(|| env.git_diff_tool.clone()) {
        t
    } else {
        select_configured_tool(config, use_gui)?
    };

    if !valid_tool(config, &tool_name) {
        return Err(Error::Message(format!("Unknown diff tool {tool_name}")));
    }

    let tool_cmd = get_tool_cmd(config, &tool_name);
    let path_key = format!("difftool.{tool_name}.path");
    let merge_path_key = format!("mergetool.{tool_name}.path");
    let tool_path = config
        .get(&path_key)
        .or_else(|| config.get(&merge_path_key))
        .or_else(|| Some(tool_name.clone()));

    Ok(ToolContext {
        tool_name,
        extcmd: None,
        tool_cmd,
        tool_path,
    })
}

fn select_configured_tool(config: &ConfigSet, use_gui: bool) -> Result<String> {
    let keys: &[&str] = if use_gui {
        &["diff.guitool", "merge.guitool", "diff.tool", "merge.tool"]
    } else {
        &["diff.tool", "merge.tool"]
    };
    for key in keys {
        if let Some(val) = config.get(key).filter(|s| !s.is_empty()) {
            if valid_tool(config, &val) {
                return Ok(val);
            }
        }
    }
    Ok("vimdiff".to_string())
}

fn get_tool_cmd(config: &ConfigSet, tool: &str) -> Option<String> {
    config
        .get(&format!("difftool.{tool}.cmd"))
        .or_else(|| config.get(&format!("mergetool.{tool}.cmd")))
}

fn valid_tool(config: &ConfigSet, tool: &str) -> bool {
    if get_tool_cmd(config, tool).is_some() {
        return true;
    }
    let path_key = format!("difftool.{tool}.path");
    let merge_path_key = format!("mergetool.{tool}.path");
    if let Some(path) = config
        .get(&path_key)
        .or_else(|| config.get(&merge_path_key))
    {
        if Command::new("sh")
            .arg("-c")
            .arg(format!("type {} >/dev/null 2>&1", shell_quote(&path)))
            .status()
            .ok()
            .is_some_and(|s| s.success())
        {
            return true;
        }
    }
    which_tool_executable(tool).is_some()
}

fn which_tool_executable(tool: &str) -> Option<String> {
    if Command::new("sh")
        .arg("-c")
        .arg(format!("type {tool} >/dev/null 2>&1"))
        .status()
        .ok()
        .is_some_and(|s| s.success())
    {
        return Some(tool.to_string());
    }
    None
}

fn collect_diff_entries(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    diff_argv: &[String],
) -> Result<Vec<DiffEntry>> {
    let mut cached = false;
    let mut revs = Vec::new();
    let mut paths = Vec::new();
    let mut in_paths = false;
    for arg in diff_argv {
        if in_paths {
            paths.push(arg.clone());
            continue;
        }
        if arg == "--" {
            in_paths = true;
            continue;
        }
        match arg.as_str() {
            "--cached" | "--staged" => cached = true,
            _ if arg.starts_with('-') => {}
            _ => revs.push(arg.clone()),
        }
    }

    let head_tree = head_tree_oid(repo).ok();
    let entries = match (cached, revs.len()) {
        (true, 0) => diff_index_to_tree(&repo.odb, index, head_tree.as_ref(), false)?,
        (true, 1) => {
            let tree = commit_or_tree_oid(repo, &revs[0])?;
            diff_index_to_tree(&repo.odb, index, Some(&tree), false)?
        }
        (false, 0) => diff_index_to_worktree(&repo.odb, index, work_tree, false, false)?,
        (false, 1) => {
            let tree = commit_or_tree_oid(repo, &revs[0])?;
            diff_tree_to_worktree(&repo.odb, Some(&tree), work_tree, index)?
        }
        (false, 2) => {
            let t1 = commit_or_tree_oid(repo, &revs[0])?;
            let t2 = commit_or_tree_oid(repo, &revs[1])?;
            diff_trees(&repo.odb, Some(&t1), Some(&t2), "")?
        }
        _ => {
            return Err(Error::Message("too many revisions for difftool".into()));
        }
    };

    let entries = entries
        .into_iter()
        .filter(|entry| entry.status != DiffStatus::Unmerged)
        .collect();
    let paths = normalize_pathspecs(work_tree, &paths);
    Ok(filter_paths(entries, &paths))
}

fn normalize_pathspecs(work_tree: &Path, paths: &[String]) -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| work_tree.to_path_buf());
    let prefix = cwd
        .strip_prefix(work_tree)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|p| !p.is_empty());
    paths
        .iter()
        .map(|path| {
            if path == "." {
                return prefix.clone().unwrap_or_else(|| ".".to_string());
            }
            if Path::new(path).is_absolute() {
                return path.clone();
            }
            match &prefix {
                Some(prefix) => format!("{prefix}/{path}"),
                None => path.clone(),
            }
        })
        .collect()
}

fn filter_paths(entries: Vec<DiffEntry>, paths: &[String]) -> Vec<DiffEntry> {
    if paths.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| {
            let p = e.path();
            paths
                .iter()
                .any(|f| p == f || p.starts_with_bytes(format!("{f}/").as_bytes()))
        })
        .collect()
}

fn apply_rotate_skip(
    mut entries: Vec<DiffEntry>,
    rotate_to: Option<&str>,
    skip_to: Option<&str>,
) -> Result<Vec<DiffEntry>> {
    if let Some(target) = rotate_to {
        let pos = entries
            .iter()
            .position(|e| e.path() == target)
            .ok_or_else(|| Error::Message(format!("File '{target}' not in diff list")))?;
        let mut tail = entries.split_off(pos);
        tail.append(&mut entries);
        entries = tail;
    }
    if let Some(target) = skip_to {
        let pos = entries
            .iter()
            .position(|e| e.path() == target)
            .ok_or_else(|| Error::Message(format!("File '{target}' not in diff list")))?;
        entries = entries.split_off(pos);
    }
    Ok(entries)
}

fn head_tree_oid(repo: &Repository) -> Result<ObjectId> {
    let head = resolve_head(&repo.git_dir)?;
    let Some(oid) = head.oid() else {
        return Err(Error::Message("unborn HEAD".into()));
    };
    peel_to_tree(repo, *oid)
}

fn commit_or_tree_oid(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid = resolve_revision(repo, spec).map_err(|e| Error::Message(e.to_string()))?;
    peel_to_tree(repo, oid)
}

fn launch_file_diff(
    repo: &Repository,
    entry: &DiffEntry,
    work_tree: &Path,
    tmp_dir: &Path,
    tool: &ToolContext,
    counter: usize,
    total: usize,
    should_prompt: bool,
    trust_exit_code: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
) -> Result<i32> {
    let merged = entry.path();
    let (local_path, remote_path) = materialize_pair(repo, entry, work_tree, tmp_dir)?;

    if should_prompt {
        writeln!(stdout)?;
        writeln!(stdout, "Viewing ({counter}/{total}): '{}'", merged.display())?;
        let prompt_label = tool.extcmd.as_deref().unwrap_or(&tool.tool_name);
        write!(stdout, "Launch '{prompt_label}' [Y/n]? ")?;
        stdout.flush().map_err(Error::Io)?;
        let mut line = String::new();
        if stdin.read_line(&mut line).ok().filter(|n| *n > 0).is_none() {
            return Ok(0);
        }
        let ans = line.trim();
        if ans.eq_ignore_ascii_case("n") || ans.eq_ignore_ascii_case("no") {
            return Ok(0);
        }
    }

    let status = run_tool(tool, &local_path, &remote_path, &merged.to_string_lossy(), counter, total)?;
    let mut code = status.code().unwrap_or(1);
    if code == 127 {
        code = 128;
    }
    if trust_exit_code && code != 0 {
        return Ok(code);
    }
    if code >= 126 {
        return Ok(code);
    }
    Ok(0)
}

fn materialize_pair(
    repo: &Repository,
    entry: &DiffEntry,
    work_tree: &Path,
    tmp_dir: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let safe_name = entry.path().to_string_lossy().replace('/', "_");
    let local_tmp = tmp_dir.join(format!("local_{safe_name}"));
    let remote_tmp = tmp_dir.join(format!("remote_{safe_name}"));

    match entry.status {
        DiffStatus::Added => {
            write_blob_or_empty(&repo.odb, &ObjectId::zero(), &local_tmp)?;
            write_blob_or_empty(&repo.odb, &entry.new_oid, &remote_tmp)?;
            Ok((local_tmp, remote_tmp))
        }
        DiffStatus::Deleted => {
            write_blob_or_empty(&repo.odb, &entry.old_oid, &local_tmp)?;
            Ok((local_tmp, PathBuf::from("/dev/null")))
        }
        _ => {
            write_blob_or_empty(&repo.odb, &entry.old_oid, &local_tmp)?;
            let wt = entry.path().to_fs_path(work_tree);
            if wt.exists() {
                Ok((local_tmp, wt))
            } else {
                write_blob_or_empty(&repo.odb, &entry.new_oid, &remote_tmp)?;
                Ok((local_tmp, remote_tmp))
            }
        }
    }
}

fn write_blob_or_empty(odb: &Odb, oid: &ObjectId, dest: &Path) -> Result<()> {
    if oid.is_zero() {
        std::fs::write(dest, "").map_err(Error::Io)?;
        return Ok(());
    }
    let data = odb.read(oid)?;
    std::fs::write(dest, &data.data).map_err(Error::Io)?;
    Ok(())
}

fn run_tool(
    tool: &ToolContext,
    local: &Path,
    remote: &Path,
    merged: &str,
    counter: usize,
    total: usize,
) -> Result<std::process::ExitStatus> {
    if let Some(extcmd) = &tool.extcmd {
        let append_pair = !extcmd.contains(char::is_whitespace);
        let script = format!(
            "export LOCAL={local} REMOTE={remote} MERGED={merged} BASE={merged}; \
             export GIT_DIFF_PATH_COUNTER={counter} GIT_DIFF_PATH_TOTAL={total} GIT_PREFIX=.; \
             set -- \"$MERGED\" \"$LOCAL\" \"$REMOTE\"; \
             cmd={cmd}; \
             if test {append_pair} = true; then \
                 eval \"$cmd\" \"$LOCAL\" \"$REMOTE\"; \
             else \
                 eval \"$cmd\"; \
             fi",
            local = shell_quote(&local.display().to_string()),
            remote = shell_quote(&remote.display().to_string()),
            merged = shell_quote(merged),
            cmd = shell_quote(extcmd),
            append_pair = if append_pair { "true" } else { "false" },
        );
        return Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdout(Stdio::inherit())
            .status()
            .map_err(Error::Io);
    }

    if let Some(tool_cmd) = &tool.tool_cmd {
        let script = format!(
            "export LOCAL={local} REMOTE={remote} MERGED={merged} BASE={merged}; \
             export GIT_DIFF_PATH_COUNTER={counter} GIT_DIFF_PATH_TOTAL={total} GIT_PREFIX=.; \
             export merge_tool={name} merge_tool_path={path}; \
             eval {tool_cmd}",
            local = shell_quote(&local.display().to_string()),
            remote = shell_quote(&remote.display().to_string()),
            merged = shell_quote(merged),
            name = shell_quote(&tool.tool_name),
            path = shell_quote(tool.tool_path.as_deref().unwrap_or(&tool.tool_name)),
            tool_cmd = tool_cmd,
        );
        return Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdout(Stdio::inherit())
            .status()
            .map_err(Error::Io);
    }

    let exe = tool.tool_path.as_deref().unwrap_or(&tool.tool_name);
    Command::new(exe)
        .arg(local)
        .arg(remote)
        .stdout(Stdio::inherit())
        .status()
        .map_err(Error::Io)
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '%' | '+' | '-' | '_' | '.' | '/'))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn run_dir_diff(
    repo: &Repository,
    entries: &[DiffEntry],
    work_tree: &Path,
    index: &Index,
    tool: &ToolContext,
    opts: &DifftoolOptions,
    _env: &DifftoolEnv,
    config: &ConfigSet,
    trust_exit_code: bool,
    should_prompt: bool,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
) -> Result<DifftoolResult> {
    let tmp = difftool_tempdir()?;
    let left = tmp.path().join("left");
    let right = tmp.path().join("right");
    std::fs::create_dir_all(&left).map_err(Error::Io)?;
    std::fs::create_dir_all(&right).map_err(Error::Io)?;

    let use_symlinks = opts
        .symlinks
        .or_else(|| config.get_bool("core.symlinks").and_then(|r| r.ok()))
        .unwrap_or(true);

    for entry in entries {
        populate_dir_side(repo, &left, entry, true, work_tree, index, use_symlinks)?;
        populate_dir_side(repo, &right, entry, false, work_tree, index, use_symlinks)?;
    }
    let right_baseline = if use_symlinks {
        BTreeMap::new()
    } else {
        capture_dir_diff_baseline(&right, entries)
    };

    if should_prompt {
        let prompt_label = tool.extcmd.as_deref().unwrap_or(&tool.tool_name);
        write!(stdout, "Launch '{prompt_label}' [Y/n]? ")?;
        stdout.flush().map_err(Error::Io)?;
        let mut line = String::new();
        if stdin.read_line(&mut line).ok().filter(|n| *n > 0).is_none() {
            return Ok(DifftoolResult { exit_code: 0 });
        }
        let ans = line.trim();
        if ans.eq_ignore_ascii_case("n") || ans.eq_ignore_ascii_case("no") {
            return Ok(DifftoolResult { exit_code: 0 });
        }
    }

    let status = if let Some(extcmd) = &tool.extcmd {
        let script = format!(
            "export LOCAL={} REMOTE={}; export GIT_DIFFTOOL_DIRDIFF=true; \
             set -- . \"$LOCAL\" \"$REMOTE\"; eval {} \"$LOCAL\" \"$REMOTE\"",
            shell_quote(&left.display().to_string()),
            shell_quote(&right.display().to_string()),
            extcmd,
        );
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::inherit())
            .status()
            .map_err(Error::Io)?
    } else if let Some(tool_cmd) = &tool.tool_cmd {
        let script = format!(
            "export LOCAL={} REMOTE={} MERGED=. BASE=.; export GIT_DIFFTOOL_DIRDIFF=true; \
             export merge_tool={} merge_tool_path={}; eval {}",
            shell_quote(&left.display().to_string()),
            shell_quote(&right.display().to_string()),
            shell_quote(&tool.tool_name),
            shell_quote(tool.tool_path.as_deref().unwrap_or(&tool.tool_name)),
            tool_cmd,
        );
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::inherit())
            .status()
            .map_err(Error::Io)?
    } else {
        let exe = tool.tool_path.as_deref().unwrap_or(&tool.tool_name);
        Command::new(exe)
            .arg(&left)
            .arg(&right)
            .stdout(Stdio::inherit())
            .status()
            .map_err(Error::Io)?
    };

    let code = status.code().unwrap_or(1);
    if !use_symlinks {
        if let Err(err) = sync_dir_diff_right_to_worktree(&right, work_tree, &right_baseline) {
            let _ = tmp.keep();
            return Err(err);
        }
    }
    if code >= 126 {
        return Ok(DifftoolResult { exit_code: code });
    }
    if trust_exit_code && code != 0 {
        return Ok(DifftoolResult { exit_code: code });
    }
    Ok(DifftoolResult { exit_code: 0 })
}

fn capture_dir_diff_baseline(
    right: &Path,
    entries: &[DiffEntry],
) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut baseline = BTreeMap::new();
    for entry in entries {
        let Some(path) = entry.new_path.as_deref().or(entry.old_path.as_deref()) else {
            continue;
        };
        baseline.insert(
            path.to_string_lossy().into_owned(),
            std::fs::read(path.to_fs_path(right)).ok(),
        );
    }
    baseline
}

fn sync_dir_diff_right_to_worktree(
    right: &Path,
    work_tree: &Path,
    baseline: &BTreeMap<String, Option<Vec<u8>>>,
) -> Result<()> {
    let mut conflict = false;
    for (rel, before) in baseline {
        let right_path = right.join(rel);
        let Ok(after) = std::fs::read(&right_path) else {
            continue;
        };
        if before.as_ref() == Some(&after) {
            continue;
        }
        let wt_path = work_tree.join(rel);
        let wt_now = std::fs::read(&wt_path).ok();
        if wt_now != *before {
            conflict = true;
            continue;
        }
        if let Some(parent) = wt_path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        std::fs::write(&wt_path, after).map_err(Error::Io)?;
    }
    if conflict {
        return Err(Error::Message(
            "working tree file changed during difftool".into(),
        ));
    }
    Ok(())
}

fn populate_dir_side(
    repo: &Repository,
    dir: &Path,
    entry: &DiffEntry,
    is_left: bool,
    work_tree: &Path,
    index: &Index,
    use_symlinks: bool,
) -> Result<()> {
    let path = if is_left {
        entry.old_path.as_deref().or(entry.new_path.as_deref())
    } else {
        entry.new_path.as_deref().or(entry.old_path.as_deref())
    };
    let Some(rel) = path else {
        return Ok(());
    };
    let dest = rel.to_fs_path(dir);

    let mode_str = if is_left {
        &entry.old_mode
    } else {
        &entry.new_mode
    };
    let oid = if is_left {
        &entry.old_oid
    } else {
        &entry.new_oid
    };

    if mode_str == "160000" {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let label = if oid.is_zero() {
            "Subproject commit 0000000000000000000000000000000000000000"
        } else {
            &format!("Subproject commit {}", oid.to_hex())
        };
        std::fs::write(&dest, label).map_err(Error::Io)?;
        return Ok(());
    }

    if mode_str.starts_with("120000") {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let wt_symlink = rel.to_fs_path(work_tree);
        let target = if oid.is_zero() || (!is_left && use_symlinks && wt_symlink.is_symlink()) {
            std::fs::read_link(rel.to_fs_path(work_tree))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            match repo.odb.read(oid) {
                Ok(blob) => String::from_utf8_lossy(&blob.data).into_owned(),
                Err(_) if !is_left && wt_symlink.is_symlink() => std::fs::read_link(wt_symlink)
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(Error::Io)?,
                Err(err) => return Err(err),
            }
        };
        std::fs::write(&dest, format!("{target}\n")).map_err(Error::Io)?;
        return Ok(());
    }

    if oid.is_zero() {
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }

    // Symlink-backed difftool checkout is a Unix optimisation; on Windows we fall
    // through to copying the work-tree file into the temp dir below.
    #[cfg(unix)]
    if !is_left && use_symlinks {
        let wt = rel.to_fs_path(work_tree);
        if wt.is_file() {
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(&wt, &dest).map_err(Error::Io)?;
            return Ok(());
        }
    }
    #[cfg(not(unix))]
    let _ = use_symlinks;

    if !is_left {
        let wt = rel.to_fs_path(work_tree);
        if wt.is_file() {
            std::fs::copy(wt, &dest).map_err(Error::Io)?;
            return Ok(());
        }
    }

    let data = repo.odb.read(oid)?;
    std::fs::write(&dest, &data.data).map_err(Error::Io)?;

    // Copy working-tree modifications for right side when applicable.
    if !is_left {
        let wt = rel.to_fs_path(work_tree);
        if wt.exists() {
            if let Ok(bytes) = std::fs::read(&wt) {
                std::fs::write(&dest, bytes).map_err(Error::Io)?;
            }
        } else if let Some(idx) = index.get(rel.as_bytes(), 0) {
            if !idx.oid.is_zero() {
                let data = repo.odb.read(&idx.oid)?;
                std::fs::write(&dest, &data.data).map_err(Error::Io)?;
            }
        }
    }
    Ok(())
}

fn difftool_tempdir() -> Result<tempfile::TempDir> {
    let Some(raw) = std::env::var_os("TMPDIR") else {
        return tempfile::tempdir().map_err(Error::Io);
    };
    let cleaned = PathBuf::from(raw.to_string_lossy().trim_end_matches('/').to_string());
    if cleaned.as_os_str().is_empty() {
        return tempfile::tempdir().map_err(Error::Io);
    }
    tempfile::Builder::new()
        .tempdir_in(cleaned)
        .map_err(Error::Io)
}

fn run_no_index_difftool(
    opts: &DifftoolOptions,
    env: &DifftoolEnv,
    config: &ConfigSet,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
) -> Result<DifftoolResult> {
    let mut paths = Vec::new();
    let mut seen_no_index = false;
    for arg in &opts.diff_argv {
        if arg == "--no-index" {
            seen_no_index = true;
            continue;
        }
        if !arg.starts_with('-') {
            paths.push(arg.clone());
        }
    }
    if !seen_no_index || paths.len() != 2 {
        return Err(Error::Message(
            "difftool --no-index requires exactly two paths".into(),
        ));
    }
    let tool_ctx = resolve_tool_context(opts, env, config)?;
    let local = PathBuf::from(&paths[0]);
    let remote = PathBuf::from(&paths[1]);
    let should_prompt = resolve_should_prompt(opts, env, config);
    if should_prompt {
        write!(stdout, "Launch '{}' [Y/n]? ", tool_ctx.tool_name)?;
        stdout.flush().map_err(Error::Io)?;
        let mut line = String::new();
        if stdin.read_line(&mut line).ok().filter(|n| *n > 0).is_none() {
            return Ok(DifftoolResult { exit_code: 0 });
        }
    }
    let status = run_tool(
        &tool_ctx,
        &local,
        &remote,
        local.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        1,
        1,
    )?;
    let code = status.code().unwrap_or(1);
    if code == 0 && paths_differ(&local, &remote) {
        return Ok(DifftoolResult { exit_code: 1 });
    }
    Ok(DifftoolResult { exit_code: code })
}

fn paths_differ(left: &Path, right: &Path) -> bool {
    match (std::fs::read(left), std::fs::read(right)) {
        (Ok(left), Ok(right)) => left != right,
        _ => true,
    }
}

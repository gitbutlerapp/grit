//! `grit hook` — list and run git hooks (Git-compatible multihooks).
//!
//! Usage:
//!   git hook run [--ignore-missing] [--to-stdin=<path>] <hook-name> [-- <hook-args>...]
//!   git hook list [-z] <hook-name>

#![allow(clippy::let_unit_value)]
use anyhow::Result;
use grit_lib::config::ConfigSet;
use grit_lib::hooks::{list_hooks_display_lines, run_hook_opts, HookResult, RunHookOptions};
use grit_lib::repo::Repository;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

fn usage_hook_top() -> ! {
    eprintln!("usage: git hook run [--ignore-missing] [--to-stdin=<path>] <hook-name> [-- <hook-args>...]");
    eprintln!("   or: git hook list [-z] <hook-name>");
    process::exit(129);
}

fn usage_hook_run() -> ! {
    eprintln!("usage: git hook run [--ignore-missing] [--to-stdin=<path>] <hook-name> [-- <hook-args>...]");
    process::exit(129);
}

fn usage_hook_list() -> ! {
    eprintln!("usage: git hook list [-z] <hook-name>");
    process::exit(129);
}

/// Entry point: `rest` is everything after `git hook`.
pub fn run_from_argv(rest: &[String]) -> Result<()> {
    let Some(sub) = rest.first().map(|s| s.as_str()) else {
        usage_hook_top();
    };
    match sub {
        "run" => run_cmd(&rest[1..]),
        "list" => list_cmd(&rest[1..]),
        _ => {
            eprintln!("error: unknown option `{sub}`");
            process::exit(129);
        }
    }
}

fn load_config(git_dir: Option<&Path>) -> Result<ConfigSet> {
    ConfigSet::load(git_dir, true).map_err(|e| anyhow::anyhow!(e))
}

fn discover_repo() -> Result<Repository> {
    Repository::discover(None).map_err(|e| anyhow::anyhow!(e))
}

/// Git still discovers a repository for `git hook run` even when `GIT_CEILING_DIRECTORIES`
/// blocks normal commands (`nongit` in the test suite); match that by ignoring the ceiling here.
struct CeilingDirectoriesGuard {
    previous: Option<String>,
}

impl CeilingDirectoriesGuard {
    fn unset() -> Self {
        let previous = std::env::var("GIT_CEILING_DIRECTORIES").ok();
        let _ = std::env::remove_var("GIT_CEILING_DIRECTORIES");
        Self { previous }
    }
}

impl Drop for CeilingDirectoriesGuard {
    fn drop(&mut self) {
        if let Some(ref s) = self.previous {
            std::env::set_var("GIT_CEILING_DIRECTORIES", s);
        }
    }
}

fn discover_repo_for_hook_run() -> Result<Repository> {
    let _ceiling = CeilingDirectoriesGuard::unset();
    discover_repo()
}

fn list_cmd(rest: &[String]) -> Result<()> {
    let mut nul = false;
    let mut pos: Vec<&str> = Vec::new();
    let mut i = 0usize;
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "-z" => {
                nul = true;
                i += 1;
            }
            "-h" | "--help" => usage_hook_list(),
            _ if a.starts_with('-') => {
                eprintln!("error: unknown option `{a}`");
                process::exit(129);
            }
            _ => {
                pos.push(a);
                i += 1;
            }
        }
    }
    if pos.len() != 1 {
        usage_hook_list();
    }
    let hook_event = pos[0];

    let repo_result = discover_repo();
    let (repo_opt, config) = match repo_result {
        Ok(r) => {
            let cfg = load_config(Some(&r.git_dir))?;
            (Some(r), cfg)
        }
        Err(_) => {
            let cfg = load_config(None)?;
            (None, cfg)
        }
    };

    let lines = match list_hooks_display_lines(repo_opt.as_ref(), hook_event, &config) {
        Ok(l) => l,
        Err(msg) => {
            eprintln!("fatal: {msg}");
            process::exit(1);
        }
    };

    if lines.is_empty() {
        eprintln!("warning: No hooks found for event '{hook_event}'");
        process::exit(1);
    }

    let mut out = std::io::stdout().lock();
    if nul {
        for line in &lines {
            let _ = write!(out, "{line}\0");
        }
    } else {
        for line in &lines {
            let _ = writeln!(out, "{line}");
        }
    }
    Ok(())
}

fn run_cmd(rest: &[String]) -> Result<()> {
    let mut ignore_missing = false;
    let mut path_to_stdin: Option<PathBuf> = None;
    let mut i = 0usize;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "-h" || a == "--help" {
            usage_hook_run();
        }
        if a == "--ignore-missing" {
            ignore_missing = true;
            i += 1;
            continue;
        }
        if let Some(path) = a.strip_prefix("--to-stdin=") {
            path_to_stdin = Some(PathBuf::from(path));
            i += 1;
            continue;
        }
        if a == "--to-stdin" {
            i += 1;
            let Some(p) = rest.get(i) else {
                usage_hook_run();
            };
            path_to_stdin = Some(PathBuf::from(p));
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            eprintln!("error: unknown option `{a}`");
            process::exit(129);
        }
        break;
    }

    let Some(hook_name) = rest.get(i).map(|s| s.as_str()) else {
        usage_hook_run();
    };
    i += 1;

    let hook_args: Vec<String> = if i >= rest.len() {
        Vec::new()
    } else {
        let dash = rest[i].as_str();
        if dash != "--" && dash != "--end-of-options" {
            usage_hook_run();
        }
        rest[i + 1..].to_vec()
    };

    let hook_args_ref: Vec<&str> = hook_args.iter().map(|s| s.as_str()).collect();

    let repo_result = discover_repo_for_hook_run();
    let (repo, config) = match repo_result {
        Ok(r) => {
            let cfg = load_config(Some(&r.git_dir))?;
            (Some(r), cfg)
        }
        Err(_) => {
            let cfg = load_config(None)?;
            (None, cfg)
        }
    };

    let stdout_to_stderr = hook_name != "pre-push";
    let stdin_path = path_to_stdin.as_deref();

    match run_hook_opts(
        repo.as_ref(),
        hook_name,
        &hook_args_ref,
        &config,
        RunHookOptions {
            stdout_to_stderr,
            path_to_stdin: stdin_path,
            stdin_data: None,
            env_vars: &[],
            cwd: None,
            commit_env: None,
        },
        None,
    ) {
        Ok(HookResult::Success) => Ok(()),
        Ok(HookResult::NotFound) if ignore_missing => Ok(()),
        Ok(HookResult::NotFound) => {
            eprintln!("error: cannot find a hook named {hook_name}");
            process::exit(1);
        }
        Ok(HookResult::Failed(code)) => process::exit(code),
        Err(msg) => {
            eprintln!("fatal: {msg}");
            process::exit(1);
        }
    }
}

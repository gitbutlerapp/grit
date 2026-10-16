//! `grit ignore check` vs `git check-ignore --stdin`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use std::process::Command;

use crate::fixture::create_repo;
use crate::scenarios::{build_report, run_scenario, Driver, RunConfig, Scenario};

fn sh_script(script: String) -> Vec<String> {
    vec!["-c".into(), script]
}
use crate::shell::shell_command;

fn shell_quote_path(path: &Path) -> String {
    shell_words::quote(path.to_string_lossy().as_ref()).into_owned()
}

fn sh_redirect_stdin(cmd: &str, input: &Path) -> String {
    format!("{cmd} < {}", shell_quote_path(input))
}

fn write_ignore_bench_paths(repo: &Path, file_count: usize) -> Result<PathBuf> {
    let list_path = repo.join("bench-ignore-paths.txt");
    let mut f = fs::File::create(&list_path).context("create path list")?;
    let files_per_dir = 100;
    let mut created = 0;
    let mut dir = 0;
    while created < file_count {
        for f_idx in 0..files_per_dir {
            if created >= file_count {
                break;
            }
            writeln!(f, "d{dir:04}/f{f_idx:04}.txt")?;
            created += 1;
        }
        dir += 1;
    }
    Ok(list_path)
}

fn grit_ignore_argv(paths: &[String]) -> Vec<String> {
    let mut argv = vec!["ignore".to_owned(), "check".to_owned(), "--all".to_owned()];
    argv.extend(paths.iter().cloned());
    argv
}

/// Benchmark checking 1000 repository paths against exclude rules.
pub fn run_ignore_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    timestamp: time::OffsetDateTime,
) -> Result<crate::schema::BenchReport> {
    const COUNT: usize = 1000;
    let repo = create_repo(git, COUNT)?;
    let out = Command::new(git)
        .args(["rm", "-r", "--cached", "."])
        .current_dir(&repo)
        .output()
        .context("git rm --cached")?;
    if !out.status.success() {
        anyhow::bail!("git rm --cached failed");
    }
    fs::write(repo.join(".gitignore"), "*.txt\n").context("write gitignore")?;
    let list_path = write_ignore_bench_paths(&repo, COUNT)?;
    let list_text = fs::read_to_string(&list_path).context("read path list")?;
    let paths: Vec<String> = list_text.lines().map(str::to_owned).collect();

    let git_inner = shell_command(git, &["check-ignore", "-v", "--non-matching", "--stdin"]);
    let git_cmd = sh_redirect_stdin(&git_inner, &list_path);

    let grit_argv = grit_ignore_argv(&paths);

    let scenario = run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "ignore-check-1000".into(),
            group: "ignore".into(),
            fixture: format!("synthetic-{COUNT}"),
            description: "check-ignore on 1000 paths (git --stdin vs grit ignore check --all)"
                .into(),
            grit_argv,
            git_argv: sh_script(git_cmd),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: true,
        },
        &repo,
    )?;

    Ok(build_report(git, grit, timestamp, vec![scenario]))
}

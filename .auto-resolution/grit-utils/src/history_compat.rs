//! Correctness checks: grit-bench `lib-op` output vs system `git`.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectId;

use crate::bench_env::{empty_global_config_path, isolated_env_prefix};
use crate::history_fixture::ensure_head_oid_list;
use crate::shell::shell_command;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainLine {
    final_line: usize,
    commit: ObjectId,
    original_line: usize,
}

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn git_output(git: &Path, dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new(git);
    cmd.args(args).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn lib_op_out(bench_exe: &Path, sub: &str, dir: &Path, extra: &[&str]) -> Result<Vec<u8>> {
    let dir_s = dir.to_string_lossy();
    let mut args = vec!["lib-op", sub, dir_s.as_ref()];
    args.extend_from_slice(extra);
    let out = Command::new(bench_exe)
        .args(&args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .with_context(|| format!("grit-bench lib-op {sub}"))?;
    if !out.status.success() {
        bail!(
            "lib-op {sub} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn lib_op_batch_out(bench_exe: &Path, dir: &Path, oid_list: &Path) -> Result<Vec<u8>> {
    let inner = format!(
        "{} lib-op cat-file-batch {} < {}",
        shell_quote(bench_exe),
        shell_quote_path(dir),
        shell_quote_path(oid_list)
    );
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&inner)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .context("lib-op cat-file-batch shell")?;
    if !out.status.success() {
        bail!(
            "lib-op cat-file-batch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn git_batch_out(git: &Path, dir: &Path, oid_list: &Path) -> Result<Vec<u8>> {
    let git_inner = shell_command(git, &["cat-file", "--batch"]);
    let inner = format!("{git_inner} < {}", shell_quote_path(oid_list));
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&inner)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .context("git cat-file --batch shell")?;
    if !out.status.success() {
        bail!(
            "git cat-file --batch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn shell_quote_path(path: &Path) -> String {
    shell_words::quote(path.to_string_lossy().as_ref()).into_owned()
}

fn shell_quote(path: &Path) -> String {
    shell_words::quote(path.to_string_lossy().as_ref()).into_owned()
}

fn parse_git_blame_porcelain(output: &[u8]) -> Result<Vec<PorcelainLine>> {
    let text = std::str::from_utf8(output).context("blame utf-8")?;
    let mut lines = Vec::new();
    let mut group: Option<(ObjectId, usize, usize)> = None;
    for raw in text.lines() {
        if raw.starts_with('\t') {
            let Some((commit, orig, final_line)) = group else {
                continue;
            };
            lines.push(PorcelainLine {
                final_line,
                commit,
                original_line: orig,
            });
            group = Some((commit, orig + 1, final_line + 1));
            continue;
        }
        let mut parts = raw.split_whitespace();
        let Some(sha) = parts.next() else {
            continue;
        };
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(commit) = sha.parse::<ObjectId>() else {
            continue;
        };
        let Some(orig_s) = parts.next() else {
            continue;
        };
        let Some(final_s) = parts.next() else {
            continue;
        };
        let Some(_count_s) = parts.next() else {
            continue;
        };
        let Ok(orig) = orig_s.parse::<usize>() else {
            continue;
        };
        let Ok(final_line) = final_s.parse::<usize>() else {
            continue;
        };
        group = Some((commit, orig, final_line));
    }
    lines.sort_by_key(|l| l.final_line);
    Ok(lines)
}

/// `lib-op rev-list-count-head` must match `git rev-list --count HEAD`.
pub fn verify_rev_list_count_head(git: &Path, bench_exe: &Path, repo: &Path) -> Result<()> {
    let git_n = String::from_utf8(git_output(git, repo, &["rev-list", "--count", "HEAD"])?)
        .context("git count utf-8")?
        .trim()
        .to_string();
    let grit_n = std::str::from_utf8(&lib_op_out(bench_exe, "rev-list-count-head", repo, &[])?)?
        .trim()
        .to_string();
    if git_n != grit_n {
        bail!("rev-list count mismatch: git={git_n} lib={grit_n}");
    }
    Ok(())
}

/// Blame porcelain for an inclusive 1-based line range must match system git.
pub fn verify_blame_porcelain_line_range(
    git: &Path,
    bench_exe: &Path,
    repo: &Path,
    path: &str,
    start: usize,
    end: usize,
) -> Result<()> {
    let spec = format!("{start},{end}");
    let git_out = git_output(
        git,
        repo,
        &["blame", "--porcelain", "-L", &spec, "HEAD", "--", path],
    )?;
    let grit_out = lib_op_out(
        bench_exe,
        "blame-porcelain-range",
        repo,
        &[path, spec.as_str()],
    )?;
    let git_lines = parse_git_blame_porcelain(&git_out)?;
    let grit_lines = parse_git_blame_porcelain(&grit_out)?;
    if git_lines.len() != grit_lines.len() {
        bail!(
            "blame -L line count mismatch for {path}: git={} lib={}",
            git_lines.len(),
            grit_lines.len()
        );
    }
    for (g, r) in git_lines.iter().zip(grit_lines.iter()) {
        if g.final_line != r.final_line
            || g.commit != r.commit
            || g.original_line != r.original_line
        {
            bail!(
                "blame -L mismatch at line {}: git={:?} lib={:?}",
                g.final_line,
                g,
                r
            );
        }
    }
    Ok(())
}

/// Blame porcelain line attribution must match system git.
pub fn verify_blame_porcelain(git: &Path, bench_exe: &Path, repo: &Path, path: &str) -> Result<()> {
    let git_out = git_output(git, repo, &["blame", "--porcelain", "HEAD", "--", path])?;
    let grit_out = lib_op_out(bench_exe, "blame-porcelain", repo, &[path])?;
    let git_lines = parse_git_blame_porcelain(&git_out)?;
    let grit_lines = parse_git_blame_porcelain(&grit_out)?;
    if git_lines.len() != grit_lines.len() {
        bail!(
            "blame line count mismatch for {path}: git={} lib={}",
            git_lines.len(),
            grit_lines.len()
        );
    }
    for (g, r) in git_lines.iter().zip(grit_lines.iter()) {
        if g.final_line != r.final_line
            || g.commit != r.commit
            || g.original_line != r.original_line
        {
            bail!(
                "blame mismatch at line {}: git={:?} lib={:?}",
                g.final_line,
                g,
                r
            );
        }
    }
    Ok(())
}

/// `lib-op cat-file-batch` over HEAD objects must match `git cat-file --batch` bytes.
pub fn verify_cat_file_batch_head(git: &Path, bench_exe: &Path, repo: &Path) -> Result<()> {
    let oids = ensure_head_oid_list(git, repo)?;
    let git_bytes = git_batch_out(git, repo, &oids)?;
    let grit_bytes = lib_op_batch_out(bench_exe, repo, &oids)?;
    if git_bytes != grit_bytes {
        bail!(
            "cat-file --batch mismatch (git {} bytes, lib {} bytes)",
            git_bytes.len(),
            grit_bytes.len()
        );
    }
    Ok(())
}

/// Run all lib-op correctness checks for the history group.
pub fn verify_history_lib_ops(
    git: &Path,
    bench_exe: &Path,
    repo: &Path,
    blame_path: &str,
) -> Result<()> {
    let _ = isolated_env_prefix();
    verify_rev_list_count_head(git, bench_exe, repo)?;
    verify_blame_porcelain(git, bench_exe, repo, blame_path)?;
    verify_cat_file_batch_head(git, bench_exe, repo)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn git_batch_shell_uses_resolved_git_path() {
        let git = Path::new("/opt/alternate/git");
        let inner = shell_command(git, &["cat-file", "--batch"]);
        assert!(inner.contains("/opt/alternate/git"));
        assert!(!inner.contains("'git' 'cat-file'"));
    }
}

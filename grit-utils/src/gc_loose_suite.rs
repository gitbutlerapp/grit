//! `grit gc` vs `git gc` on repositories whose objects are all loose.
//!
//! Two shapes, both built once and restored from a pristine copy before every run:
//!
//! * **history** — the deep-history fixture with its pack exploded into loose objects
//!   (what `git unpack-objects`, many commits, or unpacking fetches leave behind), so
//!   the delta search runs over every blob, tree and commit.
//! * **wide** — one commit of many small files, all loose (no delta work, many objects).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use time::OffsetDateTime;

use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::maintenance_suite::git_env;
use crate::network_fixture::{ensure_fixture, NetworkFixtureKind, NetworkProfile};
use crate::real_repo_fixture::run_git;
use crate::scenarios::{run_scenario, Driver, PrepareKind, RunConfig, Scenario};
use crate::schema::{BenchReport, ToolVersions, SCHEMA_VERSION};

/// Pristine copy of a benchmark repository, next to the repository itself.
fn pristine_path(repo: &Path) -> PathBuf {
    repo.with_extension("pristine")
}

fn ready_marker(repo: &Path) -> PathBuf {
    repo.with_extension("ready")
}

fn wide_file_count(profile: NetworkProfile) -> usize {
    match profile {
        NetworkProfile::Production => 200_000,
        NetworkProfile::Smoke => 2_000,
    }
}

fn fast_import(git: &Path, dest: &Path, payload: &[u8]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(["fast-import", "--quiet"])
        .current_dir(dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    git_env(&mut cmd);
    let mut child = cmd.spawn().context("spawn git fast-import")?;
    child
        .stdin
        .as_mut()
        .context("fast-import stdin")?
        .write_all(payload)?;
    let out = child.wait_with_output().context("wait fast-import")?;
    if !out.status.success() {
        bail!(
            "git fast-import failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Replace every pack of the bare repository at `repo` by loose objects.
fn explode_packs(git: &Path, repo: &Path) -> Result<()> {
    let pack_dir = repo.join("objects/pack");
    let packs: Vec<PathBuf> = fs::read_dir(&pack_dir)
        .context("read pack dir")?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect();
    for pack in packs {
        let bytes = fs::read(&pack).context("read pack")?;
        for entry in fs::read_dir(&pack_dir).context("read pack dir")? {
            let path = entry.context("pack dir entry")?.path();
            if path.file_stem() == pack.file_stem() {
                fs::remove_file(&path).context("remove pack file")?;
            }
        }
        let mut cmd = Command::new(git);
        cmd.args(["unpack-objects", "-q"])
            .current_dir(repo)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped());
        git_env(&mut cmd);
        let mut child = cmd.spawn().context("spawn git unpack-objects")?;
        child
            .stdin
            .as_mut()
            .context("unpack-objects stdin")?
            .write_all(&bytes)?;
        let out = child.wait_with_output().context("wait unpack-objects")?;
        if !out.status.success() {
            bail!(
                "git unpack-objects failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

/// Build (once) the pristine loose copy of `repo` with `populate`, then restore `repo`.
fn ensure_loose_repo(
    git: &Path,
    repo: &Path,
    populate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let pristine = pristine_path(repo);
    if !ready_marker(repo).is_file() {
        remove_dir_robust(&pristine);
        fs::create_dir_all(&pristine).context("create pristine repo dir")?;
        run_git(git, &pristine, &["init", "--bare", "-q", "-b", "main"])?;
        populate(&pristine)?;
        explode_packs(git, &pristine)?;
        fs::write(ready_marker(repo), b"loose\n").context("write ready marker")?;
    }
    restore_loose_repo(repo)
}

/// Make `repo` an exact copy of its pristine loose state, keeping the directory itself
/// so hyperfine's working directory stays valid.
pub fn restore_loose_repo(repo: &Path) -> Result<()> {
    let pristine = pristine_path(repo);
    if !pristine.is_dir() {
        bail!("missing pristine copy {}", pristine.display());
    }
    fs::create_dir_all(repo).context("create gc-loose repo dir")?;
    for entry in fs::read_dir(repo).context("read gc-loose repo")? {
        remove_dir_robust(&entry.context("gc-loose repo entry")?.path());
    }
    let mut src = pristine.into_os_string();
    src.push("/.");
    let out = Command::new("cp")
        .arg("-a")
        .arg(&src)
        .arg(repo)
        .output()
        .context("cp -a pristine repo")?;
    if !out.status.success() {
        bail!(
            "restoring {} failed: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Hyperfine prepare hook for the `gc-loose` scenarios.
pub fn run_prepare_gc_loose(worktree: &Path) -> Result<()> {
    restore_loose_repo(worktree)
}

fn wide_commit_stream(files: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"commit refs/heads/main\ncommitter T <t@example.com> 1000000000 +0000\ndata 5\nwide\n",
    );
    for i in 0..files {
        let body = format!("file {i}\n");
        out.extend_from_slice(
            format!(
                "M 100644 inline d{:03}/f{i}.txt\ndata {}\n{body}",
                i % 1000,
                body.len()
            )
            .as_bytes(),
        );
    }
    out.push(b'\n');
    out
}

/// `gc-loose-history` and `gc-loose-wide`: full `grit gc` vs `git gc` with every object loose.
pub fn run_gc_loose_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    smoke: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let profile = if smoke {
        NetworkProfile::Smoke
    } else {
        NetworkProfile::Production
    };
    let root = scratch_dir().join(format!("gc-loose-{}", profile.suffix()));
    fs::create_dir_all(&root).context("create gc-loose scratch dir")?;

    let deep = ensure_fixture(git, profile, NetworkFixtureKind::DeepHistory)?;
    let history = root.join("history");
    ensure_loose_repo(git, &history, |dest| {
        let src = deep.to_str().context("deep fixture path utf-8")?;
        run_git(git, dest, &["fetch", "-q", "--no-tags", src, "+main:main"])
    })?;

    let wide_files = wide_file_count(profile);
    let wide = root.join("wide");
    ensure_loose_repo(git, &wide, |dest| {
        fast_import(git, dest, &wide_commit_stream(wide_files))
    })?;

    let shapes = [
        (
            "gc-loose-history",
            &history,
            format!(
                "grit gc vs git gc: deep-history fixture ({}) with every object loose",
                deep.display()
            ),
        ),
        (
            "gc-loose-wide",
            &wide,
            format!("grit gc vs git gc: one commit of {wide_files} files, every object loose"),
        ),
    ];
    let mut scenarios = Vec::new();
    for (id, repo, description) in shapes {
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: id.into(),
                group: "maintenance".into(),
                fixture: repo.display().to_string(),
                description,
                grit_argv: vec!["gc".into()],
                git_argv: vec!["gc".into(), "--quiet".into()],
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::GcLooseReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            repo,
        )?);
    }

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch_dir())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
            hyperfine: None,
        },
        fixtures: vec![],
        scenarios,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_repo_is_built_once_and_restored_after_gc() -> Result<()> {
        let git = PathBuf::from("git");
        let dir = tempfile::tempdir()?;
        let repo = dir.path().join("wide");
        ensure_loose_repo(&git, &repo, |dest| {
            fast_import(&git, dest, &wide_commit_stream(50))
        })?;
        let count = |repo: &Path| -> Result<String> {
            let out = Command::new(&git)
                .args(["count-objects", "-v"])
                .current_dir(repo)
                .output()?;
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        assert!(
            count(&repo)?.starts_with("count: 102\n"),
            "{}",
            count(&repo)?
        );
        run_git(&git, &repo, &["gc", "-q"])?;
        assert!(count(&repo)?.starts_with("count: 0\n"));
        run_prepare_gc_loose(&repo)?;
        assert!(count(&repo)?.starts_with("count: 102\n"));
        Ok(())
    }
}

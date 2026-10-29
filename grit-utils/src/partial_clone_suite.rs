//! Local partial-clone benchmarks: a `blob:none` `file://` clone of a repository that
//! holds many packs, and the checkout that then lazily fetches every blob of `HEAD`.
//!
//! Both are served by the source's upload-pack (grit's runs in-process; Git starts
//! `git upload-pack`), so they measure filtered pack generation, object-id lazy fetches
//! and promisor-pack ingestion against Git's.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::hot_path_fixture::{pack_unpacked_objects, run_git};
use crate::hyperfine::{run_hyperfine, HyperfineRun};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::parallel_env::{parallel_env_prefix, ThreadMode};
use crate::parallel_fixture::{file_url, parallel_cache_root};
use crate::scenarios::{Driver, RunConfig};
use crate::schema::{BenchReport, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::shell::{shell_command, shell_quote};
use crate::stats::{median_ratio, timing_from_hyperfine};

const PARTIAL_CLONE_MIN_WARMUP: u32 = 3;
const PARTIAL_CLONE_MIN_RUNS: u32 = 10;

/// Bytes of each synthetic file, so blobs are worth fetching and deltifying.
const FILE_BYTES: usize = 2048;

/// Marker written once the source repository is complete.
const READY_MARKER: &str = "grit-bench-partial-source-ready";

/// A non-bare source with `file_count` files committed over `packs` commits, each
/// followed by packing only loose objects into a new packfile, so the object
/// store holds `packs` packs (a repository that took many pushes and was never
/// fully repacked).
///
/// # Errors
///
/// Returns an error when a `git` step fails.
pub fn ensure_many_pack_source(git: &Path, file_count: usize, packs: usize) -> Result<PathBuf> {
    ensure_many_pack_source_in(&parallel_cache_root(), git, file_count, packs)
}

fn loose_object_count(repo: &Path) -> usize {
    let objects = repo.join(".git/objects");
    fs::read_dir(&objects)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.len() == 2 && name != "pack" && name != "info"
                })
                .map(|shard| {
                    fs::read_dir(shard.path())
                        .map(|leaves| {
                            leaves
                                .filter_map(Result::ok)
                                .filter(|leaf| leaf.file_type().is_ok_and(|t| t.is_file()))
                                .count()
                        })
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0)
}

fn pack_file_count(repo: &Path) -> usize {
    fs::read_dir(repo.join(".git/objects/pack"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                .count()
        })
        .unwrap_or(0)
}

fn ensure_many_pack_source_in(
    root: &Path,
    git: &Path,
    file_count: usize,
    packs: usize,
) -> Result<PathBuf> {
    let packs = packs.max(1);
    let dir = root.join(format!("partial-source-{file_count}-{packs}p"));
    if dir.join(".git").join(READY_MARKER).is_file()
        && pack_file_count(&dir) == packs
        && loose_object_count(&dir) == 0
    {
        return Ok(dir);
    }
    if dir.exists() {
        remove_dir_robust(&dir);
    }
    fs::create_dir_all(&dir)?;
    run_git(git, &dir, &["init", "-q", "-b", "main"])?;
    // Git 2.45+ may run incremental repack during pack-objects when several small
    // packfiles exist; that merges packs and breaks the many-pack fixture layout.
    run_git(git, &dir, &["config", "gc.auto", "0"])?;
    run_git(git, &dir, &["config", "maintenance.auto", "false"])?;
    run_git(git, &dir, &["config", "uploadpack.allowFilter", "true"])?;
    run_git(
        git,
        &dir,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    )?;
    let per_pack = file_count.div_ceil(packs);
    for p in 0..packs {
        let sub = dir.join(format!("p{p:03}"));
        fs::create_dir_all(&sub)?;
        for f in 0..per_pack {
            let line = format!("pack {p} file {f} ");
            let body: String = line.repeat(FILE_BYTES / line.len() + 1);
            fs::write(sub.join(format!("f{f:05}.txt")), &body[..FILE_BYTES])?;
        }
        run_git(git, &dir, &["add", "-A"])?;
        run_git(git, &dir, &["commit", "-q", "-m", &format!("batch {p}")])?;
        pack_unpacked_objects(git, &dir)?;
    }
    fs::write(dir.join(".git").join(READY_MARKER), "")?;
    Ok(dir)
}

/// A `blob:none` clone of `source` made by `git` with `--no-checkout`, copied into place
/// before each lazy-fetch checkout run so both tools start from the same state.
fn ensure_blobless_template(
    git: &Path,
    url: &str,
    file_count: usize,
    packs: usize,
) -> Result<PathBuf> {
    let dir = parallel_cache_root().join(format!("partial-template-{file_count}-{packs}p"));
    if dir.join(".git").join(READY_MARKER).is_file() {
        return Ok(dir);
    }
    if dir.exists() {
        remove_dir_robust(&dir);
    }
    let parent = dir.parent().unwrap_or(Path::new("/tmp"));
    fs::create_dir_all(parent)?;
    let dest = dir.to_str().context("template path utf8")?;
    run_git(
        git,
        parent,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            url,
            dest,
        ],
    )?;
    fs::write(dir.join(".git").join(READY_MARKER), "")?;
    Ok(dir)
}

fn work_dir() -> PathBuf {
    scratch_dir().join("partial-clone")
}

/// Hyperfine grit vs git on a `blob:none` local clone of a many-pack source (without and
/// with its checkout) and on the checkout of a blobless clone, both of which lazily
/// fetch every blob of `HEAD`.
///
/// # Errors
///
/// Returns an error when the fixture cannot be built or hyperfine fails.
pub fn run_partial_clone_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    file_count: usize,
    packs: usize,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let source = ensure_many_pack_source(git, file_count, packs)?;
    let url = file_url(&source.join(".git"))?;
    let template = ensure_blobless_template(git, &url, file_count, packs)?;
    let work = work_dir();
    fs::create_dir_all(&work)?;
    let dest = work.join("clone");
    let dest_s = dest.display().to_string();
    let env = parallel_env_prefix(ThreadMode::Auto);
    let remove = format!("rm -rf {}", shell_quote(&dest_s));
    let restore = format!(
        "rm -rf {d} && cp -R {t} {d}",
        d = shell_quote(&dest_s),
        t = shell_quote(&template.display().to_string())
    );
    let in_dest = |cmd: String| format!("cd {} && {cmd}", shell_quote(&dest_s));

    let run = |command: String, prepare: &str, name: &str| {
        run_hyperfine(
            hyperfine,
            &HyperfineRun {
                command,
                working_directory: work.clone(),
                prepare: Some(prepare.to_owned()),
                warmup: cfg.warmup.max(PARTIAL_CLONE_MIN_WARMUP),
                min_runs: cfg.min_runs.max(PARTIAL_CLONE_MIN_RUNS),
                exact_runs: cfg.exact_runs,
                command_name: Some(name.to_owned()),
                env_prefix: Some(env.clone()),
            },
        )
        .map(|entry| timing_from_hyperfine(&entry))
    };

    let mut scenarios = Vec::new();
    let git_clone = run(
        shell_command(
            git,
            &[
                "clone",
                "-q",
                "--filter=blob:none",
                "--no-checkout",
                &url,
                &dest_s,
            ],
        ),
        &remove,
        "git",
    )?;
    let grit_clone = run(
        shell_command(
            grit,
            &["clone", "--blobs", "none", "--no-checkout", &url, &dest_s],
        ),
        &remove,
        "grit",
    )?;
    scenarios.push(ScenarioResult {
        id: format!("partial-clone-blobless-file-{file_count}-{packs}packs"),
        group: "partial-clone".into(),
        fixture: format!("synthetic-{file_count}-{packs}-packs"),
        description: format!(
            "blob:none file:// clone --no-checkout of a {packs}-pack source ({file_count} files)"
        ),
        driver: Driver::Cli.into(),
        ratio: median_ratio(git_clone.median_ms, grit_clone.median_ms),
        git: git_clone,
        grit: grit_clone,
        grit_failure: None,
    });

    let git_clone_checkout = run(
        shell_command(git, &["clone", "-q", "--filter=blob:none", &url, &dest_s]),
        &remove,
        "git",
    )?;
    let grit_clone_checkout = run(
        shell_command(grit, &["clone", "--blobs", "none", &url, &dest_s]),
        &remove,
        "grit",
    )?;
    scenarios.push(ScenarioResult {
        id: format!("partial-clone-blobless-checkout-{file_count}-{packs}packs"),
        group: "partial-clone".into(),
        fixture: format!("synthetic-{file_count}-{packs}-packs"),
        description: format!(
            "blob:none file:// clone with checkout of a {packs}-pack source, lazily fetching all {file_count} blobs in one request"
        ),
        driver: Driver::Cli.into(),
        ratio: median_ratio(git_clone_checkout.median_ms, grit_clone_checkout.median_ms),
        git: git_clone_checkout,
        grit: grit_clone_checkout,
        grit_failure: None,
    });

    let git_checkout = run(
        in_dest(shell_command(git, &["reset", "-q", "--hard", "HEAD"])),
        &restore,
        "git",
    )?;
    let grit_checkout = run(
        in_dest(shell_command(grit, &["reset", "--hard", "HEAD"])),
        &restore,
        "grit",
    )?;
    scenarios.push(ScenarioResult {
        id: format!("partial-clone-lazy-checkout-{file_count}-{packs}packs"),
        group: "partial-clone".into(),
        fixture: format!("synthetic-{file_count}-{packs}-packs"),
        description: format!(
            "reset --hard in a blob:none clone, lazily fetching all {file_count} blobs from a {packs}-pack file:// source"
        ),
        driver: Driver::Cli.into(),
        ratio: median_ratio(git_checkout.median_ms, grit_checkout.median_ms),
        git: git_checkout,
        grit: grit_checkout,
        grit_failure: None,
    });

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
    fn source_has_one_pack_per_batch_and_every_file() {
        let Ok(git) = crate::binary::resolve_binary("git", None) else {
            return;
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = ensure_many_pack_source_in(tmp.path(), &git, 20, 4).expect("fixture");
        let packs = fs::read_dir(dir.join(".git/objects/pack"))
            .expect("pack dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .count();
        assert_eq!(packs, 4);
        assert_eq!(
            loose_object_count(&dir),
            0,
            "objects must live in packs only"
        );
        run_git(&git, &dir, &["fsck", "--strict"]).expect("fsck");
        let files: usize = (0..4)
            .map(|p| fs::read_dir(dir.join(format!("p{p:03}"))).map_or(0, |d| d.count()))
            .sum();
        assert_eq!(files, 20);
        // Cached: a second call reuses it.
        assert_eq!(
            ensure_many_pack_source_in(tmp.path(), &git, 20, 4).expect("cached"),
            dir
        );
        // Ready marker with a stale pack layout must rebuild.
        let pack_dir = dir.join(".git/objects/pack");
        let stale_pack = fs::read_dir(&pack_dir)
            .expect("pack dir")
            .filter_map(Result::ok)
            .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .expect("a pack file");
        fs::remove_file(stale_pack.path()).expect("remove pack");
        assert_eq!(pack_file_count(&dir), 3);
        let rebuilt = ensure_many_pack_source_in(tmp.path(), &git, 20, 4).expect("rebuild");
        assert_eq!(rebuilt, dir);
        assert_eq!(pack_file_count(&dir), 4);
        assert_eq!(loose_object_count(&dir), 0);
    }
}

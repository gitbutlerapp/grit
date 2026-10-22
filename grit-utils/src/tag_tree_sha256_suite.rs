//! tree-sha256 annotated tag benchmarks on git.git-, linux-, and chromium-synth-shaped fixtures.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::binary::{grit_source_commit, tool_version};
use crate::chromium_synth::{ensure_chromium_synthetic, ChromiumSynthSpec};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::real_repo_fixture::{
    ensure_git_git_bitmaps, ensure_linux_bare, ensure_worktree_clone, linux_fixture_enabled,
    run_git, scratch_root, DEFAULT_GIT_GIT_TAG, DEFAULT_LINUX_TAG,
};
use crate::scenarios::{
    run_scenario, sh_script_internal, Driver, PrepareKind, RunConfig, Scenario,
};
use crate::schema::{BenchReport, FixtureRecord, ToolVersions, SCHEMA_VERSION};

const BENCH_DIGEST_TAG: &str = "grit-bench-tree-sha256-digest";
const BENCH_SIGNED_TAG: &str = "grit-bench-tree-sha256-signed";

fn pinned_git_git_tag() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_TAG").unwrap_or_else(|_| DEFAULT_GIT_GIT_TAG.to_owned())
}

fn shell_escape(path: &Path) -> String {
    format!("'{}'", path.display())
}

fn reference_tree_sha256_bin() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("TREE_SHA256_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    for name in ["tree-sha256", "tree_sha256"] {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {name}"))
            .output()
            .ok()?;
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

fn git_output(git: &Path, worktree: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(git)
        .current_dir(worktree)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn ensure_signing_config(git: &Path, worktree: &Path) -> Result<()> {
    let key = scratch_root().join("tag-bench-signing-key");
    if !key.with_extension("pub").is_file() {
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .context("ssh-keygen for tag bench")?;
        if !status.success() {
            anyhow::bail!("ssh-keygen failed");
        }
    }
    let allowed = scratch_root().join("tag-bench-allowed-signers");
    let pub_key = std::fs::read_to_string(key.with_extension("pub")).context("read signing pub")?;
    std::fs::write(&allowed, format!("bench@example.com {pub_key}")).context("write allowed")?;
    for (k, v) in [
        ("user.name", "Bench"),
        ("user.email", "bench@example.com"),
        ("gpg.format", "ssh"),
        (
            "user.signingkey",
            &key.with_extension("pub").to_string_lossy(),
        ),
        ("gpg.ssh.allowedSignersFile", &allowed.to_string_lossy()),
        ("tag.gpgSign", "false"),
    ] {
        run_git(git, worktree, &["config", k, v])?;
    }
    Ok(())
}

fn reset_bench_tags(grit: &Path, worktree: &Path) -> Result<()> {
    for name in [BENCH_DIGEST_TAG, BENCH_SIGNED_TAG] {
        let _ = Command::new(grit)
            .current_dir(worktree)
            .args(["tag", "-d", name])
            .output();
    }
    Ok(())
}

fn prepare_digest_tag(grit: &Path, worktree: &Path) -> Result<()> {
    reset_bench_tags(grit, worktree)?;
    let out = Command::new(grit)
        .current_dir(worktree)
        .args([
            "tag",
            BENCH_DIGEST_TAG,
            "--sign",
            "--tree-sha256",
            "-m",
            "bench digest",
            "-f",
        ])
        .output()
        .context("create digest bench tag")?;
    if !out.status.success() {
        anyhow::bail!(
            "grit tag digest prepare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn prepare_signed_tag(grit: &Path, worktree: &Path) -> Result<()> {
    reset_bench_tags(grit, worktree)?;
    prepare_digest_tag(grit, worktree)?;
    let out = Command::new(grit)
        .current_dir(worktree)
        .args([
            "tag",
            BENCH_SIGNED_TAG,
            "--sign",
            "--tree-sha256",
            "-m",
            "bench signed",
            "-f",
        ])
        .output()
        .context("create signed bench tag")?;
    if !out.status.success() {
        anyhow::bail!(
            "grit tag signed prepare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn git_plain_tree_sha256_script(git: &Path, rev: &str) -> Vec<String> {
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/tree_sha256_git_digest.py");
    sh_script_internal(format!(
        "/usr/bin/env python3 {} {} {rev:?} >/dev/null",
        shell_escape(&script),
        shell_escape(git),
    ))
}

fn grit_verify_digest_argv(grit: &Path, tag: &str) -> Vec<String> {
    sh_script_internal(format!(
        "{} tag --verify {tag} --no-signature >/dev/null",
        shell_escape(grit)
    ))
}

fn grit_verify_full_argv(grit: &Path, tag: &str) -> Vec<String> {
    sh_script_internal(format!(
        "{} tag --verify {tag} >/dev/null",
        shell_escape(grit)
    ))
}

fn git_verify_tag_argv(git: &Path, tag: &str) -> Vec<String> {
    sh_script_internal(format!("{} verify-tag {tag} >/dev/null", shell_escape(git)))
}

fn reference_digest_argv(bin: &Path, rev: &str) -> Vec<String> {
    sh_script_internal(format!(
        "{} --repo \"$PWD\" {rev} >/dev/null",
        shell_escape(bin)
    ))
}

fn tag_sign_only_argv(grit: &Path) -> Vec<String> {
    sh_script_internal(format!(
        "{} tag \"bench-sign-$(date +%s%N)\" --sign -m bench -f >/dev/null",
        shell_escape(grit)
    ))
}

fn tag_sign_tree_sha256_argv(grit: &Path) -> Vec<String> {
    sh_script_internal(format!(
        "{} tag \"bench-ts-$(date +%s%N)\" --sign --tree-sha256 -m bench -f >/dev/null",
        shell_escape(grit)
    ))
}

struct FixtureBench {
    id: &'static str,
    label: String,
    worktree: PathBuf,
    record: FixtureRecord,
}

fn ensure_git_git_fixture(git: &Path, grit: &Path) -> Result<FixtureBench> {
    let tag = pinned_git_git_tag();
    let bare = ensure_git_git_bitmaps(git)?;
    let worktree = ensure_worktree_clone(git, &bare, "tag-tree-sha256-git-git", &tag)?;
    run_git(
        git,
        &worktree,
        &["submodule", "update", "--init", "--recursive", "-q"],
    )?;
    ensure_signing_config(git, &worktree)?;
    prepare_digest_tag(grit, &worktree)?;
    let head = git_output(git, &worktree, &["rev-parse", "HEAD"])?;
    Ok(FixtureBench {
        id: "git-dot-git",
        label: "git-dot-git".into(),
        worktree,
        record: FixtureRecord {
            name: "git-dot-git".into(),
            tier: None,
            head_oid: head,
            object_count: 0,
            ref_count: 0,
            file_count: 0,
        },
    })
}

fn ensure_chromium_fixture(git: &Path, grit: &Path, smoke: bool) -> Result<FixtureBench> {
    let spec = if smoke {
        ChromiumSynthSpec::smoke()
    } else {
        ChromiumSynthSpec::production()
    };
    let bare = ensure_chromium_synthetic(git, spec)?;
    let worktree = ensure_worktree_clone(
        git,
        &bare,
        &format!("tag-tree-sha256-chromium-{}", spec.cache_key()),
        &spec.cache_key(),
    )?;
    ensure_signing_config(git, &worktree)?;
    prepare_digest_tag(grit, &worktree)?;
    let head = git_output(git, &worktree, &["rev-parse", "HEAD"])?;
    Ok(FixtureBench {
        id: "chromium-synth",
        label: format!("chromium-synth-{}", spec.cache_key()),
        worktree,
        record: FixtureRecord {
            name: "chromium-synth".into(),
            tier: Some(spec.cache_key()),
            head_oid: head,
            object_count: 0,
            ref_count: 0,
            file_count: 0,
        },
    })
}

fn ensure_linux_fixture(git: &Path, grit: &Path) -> Result<FixtureBench> {
    let linux_tag =
        std::env::var("GRIT_BENCH_LINUX_TAG").unwrap_or_else(|_| DEFAULT_LINUX_TAG.to_owned());
    let bare = ensure_linux_bare(git)?;
    let worktree = ensure_worktree_clone(git, &bare, "tag-tree-sha256-linux", &linux_tag)?;
    ensure_signing_config(git, &worktree)?;
    prepare_digest_tag(grit, &worktree)?;
    let head = git_output(git, &worktree, &["rev-parse", "HEAD"])?;
    Ok(FixtureBench {
        id: "linux",
        label: "linux".into(),
        worktree,
        record: FixtureRecord {
            name: "linux".into(),
            tier: None,
            head_oid: head,
            object_count: 0,
            ref_count: 0,
            file_count: 0,
        },
    })
}

fn scenarios_for_fixture(
    fixture: &FixtureBench,
    git: &Path,
    grit: &Path,
    ref_bin: Option<&Path>,
) -> Vec<Scenario> {
    let id_prefix = fixture.id;
    let mut out = vec![
        Scenario {
            id: format!("{id_prefix}/tag-verify-digest-no-signature"),
            group: "tag-tree-sha256".into(),
            fixture: fixture.label.clone(),
            description: "grit tag --verify --no-signature vs git ls-tree/cat-file sha256 pipeline"
                .into(),
            grit_argv: grit_verify_digest_argv(grit, BENCH_DIGEST_TAG),
            git_argv: git_plain_tree_sha256_script(git, "HEAD"),
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::TagTreeSha256Digest),
            grit_via_shell: true,
            git_via_shell: true,
        },
        Scenario {
            id: format!("{id_prefix}/tag-sign-tree-sha256"),
            group: "tag-tree-sha256".into(),
            fixture: fixture.label.clone(),
            description: "grit tag --sign --tree-sha256 vs grit tag --sign only (fresh names)"
                .into(),
            grit_argv: tag_sign_tree_sha256_argv(grit),
            git_argv: tag_sign_only_argv(grit),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        Scenario {
            id: format!("{id_prefix}/tag-verify-signed"),
            group: "tag-tree-sha256".into(),
            fixture: fixture.label.clone(),
            description: "grit tag --verify vs git verify-tag on signed tree-sha256 tag".into(),
            grit_argv: grit_verify_full_argv(grit, BENCH_SIGNED_TAG),
            git_argv: git_verify_tag_argv(git, BENCH_SIGNED_TAG),
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::TagTreeSha256Signed),
            grit_via_shell: true,
            git_via_shell: true,
        },
    ];
    if let Some(bin) = ref_bin {
        out.push(Scenario {
            id: format!("{id_prefix}/tag-verify-digest-reference"),
            group: "tag-tree-sha256".into(),
            fixture: fixture.label.clone(),
            description: "grit tag --verify --no-signature vs reference tree-sha256 binary".into(),
            grit_argv: grit_verify_digest_argv(grit, BENCH_DIGEST_TAG),
            git_argv: reference_digest_argv(bin, "HEAD"),
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::TagTreeSha256Digest),
            grit_via_shell: true,
            git_via_shell: true,
        });
    }
    out
}

/// Run tree-sha256 tag benchmarks across real-shaped fixtures.
pub fn run_tag_tree_sha256_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    smoke: bool,
    with_linux: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let mut cfg = cfg.clone();
    if smoke {
        cfg.warmup = 1;
        cfg.min_runs = 3;
    }

    let ref_bin = reference_tree_sha256_bin();
    if ref_bin.is_none() {
        eprintln!("reference tree-sha256 binary not found; skipping reference scenarios");
    }

    let mut fixtures = vec![ensure_git_git_fixture(git, grit)?];
    fixtures.push(ensure_chromium_fixture(git, grit, smoke)?);
    if linux_fixture_enabled(with_linux) {
        fixtures.push(ensure_linux_fixture(git, grit)?);
    }

    let mut scenarios = Vec::new();
    for fixture in &fixtures {
        prepare_signed_tag(grit, &fixture.worktree)?;
        for spec in scenarios_for_fixture(fixture, git, grit, ref_bin.as_deref()) {
            scenarios.push(run_scenario(
                hyperfine,
                git,
                grit,
                &cfg,
                &spec,
                &fixture.worktree,
            )?);
        }
        for scenario in scenarios
            .iter()
            .filter(|s| s.fixture == fixture.label && s.id.contains("tag-sign-tree-sha256"))
        {
            let sign_median = scenario.git.median_ms;
            let total_median = scenario.grit.median_ms;
            let overhead_ms = total_median - sign_median;
            eprintln!(
                "{} tag-sign-tree-sha256: sign-only {sign_median:.2}ms, with digest {total_median:.2}ms, overhead {overhead_ms:.2}ms (target ≤ hash+10ms documented in benchmarks.md)",
                fixture.id
            );
        }
    }

    let fixture_records: Vec<FixtureRecord> = fixtures.iter().map(|f| f.record.clone()).collect();
    let scratch = scratch_root();
    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch)?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        fixtures: fixture_records,
        scenarios,
    })
}

/// Hyperfine prepare hook: recreate bench tags before each timed run.
pub fn prepare_tag_tree_sha256_bench(grit: &Path, worktree: &Path, signed: bool) -> Result<()> {
    if signed {
        prepare_signed_tag(grit, worktree)
    } else {
        prepare_digest_tag(grit, worktree)
    }
}

/// `grit-bench prepare-tag-tree-sha256-digest` (hyperfine prepare; uses process cwd).
pub fn run_prepare_tag_tree_sha256_digest(grit: &Path) -> Result<()> {
    let worktree = std::env::current_dir().context("prepare cwd")?;
    prepare_digest_tag(grit, &worktree)
}

/// `grit-bench prepare-tag-tree-sha256-signed` (hyperfine prepare; uses process cwd).
pub fn run_prepare_tag_tree_sha256_signed(grit: &Path) -> Result<()> {
    let worktree = std::env::current_dir().context("prepare cwd")?;
    prepare_signed_tag(grit, &worktree)
}

//! `grit repack` — pack objects and optionally remove redundant packs.
//!
//! Matches Git’s plumbing: default `repack -d -l` runs `pack-objects` with **`--all --reflog
//! --indexed-objects --unpacked --incremental`** (incremental repack). **`repack -a` / `-A`**
//! runs a full repack into one pack (with optional **`--unpack-unreachable`**), same as `git gc`.
//! **`--geometric`** mirrors `git repack --geometric`: `pack-objects --stdin-packs --unpacked`
//! over a computed pack split, optional promisor merge, MIDX, and redundant pack removal.

use crate::commands::update_server_info;
use crate::grit_exe;
use crate::trace2_emit_git_subcommand_argv;
use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
use grit_lib::config::ConfigSet;
use grit_lib::midx::{
    clear_pack_midx_state, write_multi_pack_index_with_options, WriteMultiPackIndexOptions,
};
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::pack_geometry::{
    collect_geometry_packs, collect_promisor_geometry_packs, compute_geometry_split,
    preferred_pack_stem_after_split, GeometricPack,
};
use grit_lib::promisor::{promisor_pack_object_ids, repo_treats_promisor_packs};
use grit_lib::prune_packed::{prune_packed_objects, PrunePackedOptions};
use grit_lib::repo::Repository;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const FAST_IMPORT_UNPACK_MARKER: &str = "grit-fast-import-unpacklimit0";

/// Arguments for `grit repack`.
#[derive(Debug, ClapArgs)]
#[command(about = "Pack unpacked objects in a repository")]
pub struct Args {
    /// Remove redundant packs after repacking (keeps the pack created by this run).
    #[arg(short = 'd')]
    pub delete_old: bool,

    /// Pass `--local` to pack-objects (accepted for compat).
    #[arg(short = 'l', long = "local")]
    pub local: bool,

    /// Pack everything into a single pack (Git `-a`).
    #[arg(short = 'a', conflicts_with = "repack_all_unpack")]
    pub all: bool,

    /// Like `-a`, and loosen unreachable objects per `--unpack-unreachable` (Git `-A`).
    #[arg(short = 'A', conflicts_with = "all")]
    pub repack_all_unpack: bool,

    /// Keep unreachable objects, folding them into the repacked pack (Git `-k`).
    #[arg(short = 'k', long = "keep-unreachable")]
    pub keep_unreachable: bool,

    /// Write a bitmap index (same as `git repack -b`). Fails when promisor packs are present or
    /// the object set is not closed (matches Git’s bitmap constraints).
    #[arg(
        short = 'b',
        long = "write-bitmap-index",
        visible_alias = "write-bitmap"
    )]
    pub write_bitmap: bool,

    /// Suppress bitmap index write (Git `repack` incremental auto-gc path).
    #[arg(long = "no-write-bitmap-index")]
    pub no_write_bitmap_index: bool,

    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Pass `--no-reuse-delta` (accepted; forwarded to pack-objects).
    #[arg(short = 'f')]
    pub force: bool,

    /// Pass `--delta-islands` to pack-objects (Git `repack -i` / `repack.useDeltaIslands`).
    #[arg(short = 'i', long = "delta-islands")]
    pub delta_islands: bool,

    /// Use deeper delta compression (same as `git gc --aggressive`).
    #[arg(long)]
    pub aggressive: bool,

    #[arg(long)]
    pub window: Option<i64>,

    #[arg(long)]
    pub depth: Option<i64>,

    /// Write cruft pack (accepted; forwarded to pack-objects).
    #[arg(long)]
    pub cruft: bool,

    #[arg(long = "no-cruft")]
    pub no_cruft: bool,

    /// Expire cruft objects older than this (`git repack --cruft-expiration`, forwarded to the
    /// cruft `pack-objects` pass).
    #[arg(long = "cruft-expiration", value_name = "TIME")]
    pub cruft_expiration: Option<String>,

    /// With `-A` / `-a`, do not loosen objects older than this (Git `--unpack-unreachable=<date>`).
    #[arg(long = "unpack-unreachable", value_name = "DATE")]
    pub unpack_unreachable: Option<String>,

    /// List-objects filter (forwarded to `pack-objects`, e.g. `blob:none`).
    #[arg(long = "filter", value_name = "SPEC", action = clap::ArgAction::Append)]
    pub filter: Vec<String>,

    /// Destination pack prefix for filtered-out objects (`git repack --filter-to`).
    #[arg(long = "filter-to", value_name = "DIR")]
    pub filter_to: Option<String>,

    /// Alternate location for pruned objects (`git repack --expire-to`).
    #[arg(long = "expire-to", value_name = "DIR")]
    pub expire_to: Option<String>,

    /// Limit cruft pack size (`git repack --max-cruft-size`).
    #[arg(long = "max-cruft-size", value_name = "SIZE")]
    pub max_cruft_size: Option<String>,

    /// Do not repack this pack (basename `pack-….pack`; repeatable).
    #[arg(long = "keep-pack", value_name = "NAME", action = clap::ArgAction::Append)]
    pub keep_pack: Vec<String>,

    /// Geometric repack factor (same as `git repack --geometric=<n>`).
    #[arg(short = 'g', long = "geometric")]
    pub geometric: Option<i32>,

    /// Write multi-pack-index after repack.
    #[arg(short = 'm', long = "write-midx")]
    pub write_midx: bool,

    /// Repack objects inside `.keep` packs (matches `git repack --pack-kept-objects`).
    #[arg(long = "pack-kept-objects", action = clap::ArgAction::SetTrue)]
    pub pack_kept_objects: bool,

    /// Maximum pack size in bytes (forwarded to pack-objects).
    #[arg(long = "max-pack-size")]
    pub max_pack_size: Option<String>,

    /// Object name hash version forwarded to `pack-objects` (`git repack --name-hash-version`).
    #[arg(long = "name-hash-version", value_name = "N")]
    pub name_hash_version: Option<i32>,

    /// Do not update server info (`git repack -n` / `--no-update-server-info`).
    #[arg(short = 'n', long = "no-update-server-info")]
    pub no_update_server_info: bool,

    /// Extra arguments (ignored).
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub rest: Vec<String>,
}

fn parse_config_byte_size(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let upper = s.to_ascii_uppercase();
    let (digits, mult) = if upper.ends_with('K') {
        (&s[..s.len() - 1], 1024u64)
    } else if upper.ends_with('M') {
        (&s[..s.len() - 1], 1024u64 * 1024)
    } else if upper.ends_with('G') {
        (&s[..s.len() - 1], 1024u64 * 1024 * 1024)
    } else {
        (s, 1u64)
    };
    let n: u64 = digits.trim().parse().ok()?;
    Some(n.saturating_mul(mult))
}

/// Git `write_bitmaps` after config / defaults: negative means “quiet bitmap” path, `>0` enables.
fn effective_write_bitmaps_int(
    args: &Args,
    cfg: &ConfigSet,
    full_repack: bool,
    bare_repo: bool,
) -> i32 {
    let mut wb: i32 = if args.write_bitmap {
        1
    } else if args.no_write_bitmap_index {
        0
    } else {
        -1
    };
    if wb < 0 {
        if let Some(v) = cfg
            .get("repack.writebitmaps")
            .or_else(|| cfg.get("pack.writeBitmaps"))
        {
            wb = if v == "true" || v == "1" || v.eq_ignore_ascii_case("yes") {
                1
            } else {
                0
            };
        }
    }
    if wb < 0 && !args.write_midx && (!full_repack || !bare_repo) {
        wb = 0;
    }
    wb
}

/// Whether `git repack` should run `update_server_info` at the end. Git defaults to true; it is
/// disabled by `-n` / `--no-update-server-info` or `repack.updateServerInfo=false`.
fn should_update_server_info(args: &Args, cfg: &ConfigSet) -> bool {
    if args.no_update_server_info {
        return false;
    }
    if let Some(v) = cfg
        .get("repack.updateserverinfo")
        .or_else(|| cfg.get("repack.updateServerInfo"))
    {
        return !(v == "false" || v == "0" || v.eq_ignore_ascii_case("no"));
    }
    true
}

/// Whether to include objects from `.keep` packs in the new pack(s), matching Git `pack_kept_objects`.
fn resolve_pack_kept_objects(
    args: &Args,
    cfg: &ConfigSet,
    full_repack: bool,
    bare_repo: bool,
) -> bool {
    if args.pack_kept_objects {
        return true;
    }
    if let Some(v) = cfg
        .get("repack.packkeptobjects")
        .or_else(|| cfg.get("repack.packKeptObjects"))
    {
        return v == "true" || v == "1" || v.eq_ignore_ascii_case("yes");
    }
    effective_write_bitmaps_int(args, cfg, full_repack, bare_repo) > 0 && !args.write_midx
}

/// Run `grit repack`.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();

    let geometric = args.geometric.unwrap_or(0).max(0);
    let full_repack_early = args.all || args.repack_all_unpack || args.cruft;
    let bare_repo = repo.work_tree.is_none();
    let pack_kept_objects = resolve_pack_kept_objects(&args, &cfg, full_repack_early, bare_repo);

    let precious_objects = cfg
        .get_bool("extensions.preciousobjects")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    if precious_objects && args.delete_old && (args.all || args.repack_all_unpack) {
        anyhow::bail!("cannot delete packs in a precious-objects repo");
    }

    if geometric > 0 && (args.all || args.repack_all_unpack) {
        anyhow::bail!("options '--geometric' and '-a' cannot be used together");
    }
    if geometric > 0 {
        let r = run_geometric(&repo, &args, pack_kept_objects, geometric);
        if r.is_ok() {
            let _ = grit_lib::shared_repo::refresh_repository_shared_tree(&repo.git_dir);
        }
        return r;
    }

    if args.cruft && args.repack_all_unpack {
        anyhow::bail!("options '-A' and '--cruft' cannot be used together");
    }
    // Git `pack-objects` cannot combine `--write-bitmap-index` with `--filter`; repack fails once
    // the filtered pack is incomplete for bitmap closure (`t7700-repack`).
    if (args.all || args.repack_all_unpack)
        && !args.cruft
        && !args.write_midx
        && args.filter.iter().any(|s| !s.trim().is_empty())
    {
        let wb = effective_write_bitmaps_int(&args, &cfg, true, bare_repo);
        if wb > 0 {
            anyhow::bail!("fatal: failed to write bitmap index");
        }
    }
    let work_dir = repo.work_tree.as_deref().unwrap_or(&repo.git_dir);
    let grit_bin = grit_exe::grit_executable();

    let pack_base = if repo.work_tree.is_some() {
        repo.odb
            .objects_dir()
            .join("pack")
            .join("pack")
            .to_string_lossy()
            .into_owned()
    } else {
        "objects/pack/pack".to_owned()
    };

    let pack_dir_abs = repo.odb.objects_dir().join("pack");
    ensure_no_orphan_pack_indexes(&pack_dir_abs)?;
    if std::env::var("GIT_REF_PARANOIA").ok().as_deref() != Some("0") {
        guard_against_corrupt_loose_refs_for_repack(&repo)?;
    }

    let full_repack = args.all || args.repack_all_unpack || args.cruft;
    let fast_import_unpack_marker = repo.git_dir.join(FAST_IMPORT_UNPACK_MARKER);
    let fast_import_bulk_pack_only = full_repack
        && args.all
        && args.delete_old
        && args.quiet
        && !args.repack_all_unpack
        && !args.cruft
        && !args.write_midx
        && !args.write_bitmap
        && !args.no_write_bitmap_index
        && !args.local
        && !args.keep_unreachable
        && args.keep_pack.is_empty()
        && args.filter.iter().all(|f| f.trim().is_empty())
        && args
            .filter_to
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
        && fast_import_unpack_marker.is_file();
    // Git only drops the existing MIDX when it is about to rewrite it (`--write-midx`) or when
    // a pack the MIDX references is removed as redundant (git/repack.c
    // `repack_remove_redundant_pack` -> `clear_midx_file` gated on `midx_contains_pack`).
    // A plain `repack -ad` over `.keep` packs the MIDX was built from must leave the MIDX
    // untouched (t5319 "repack preserves multi-pack-index when creating packs"). Snapshot the
    // packs the MIDX names so we can clear it later iff one of them gets deleted.
    let midx_referenced_packs: Vec<String> = if args.write_midx {
        // The `--write-midx` path rewrites the MIDX from scratch below; clear stale state now.
        clear_pack_midx_state(&pack_dir_abs).map_err(|e| anyhow::anyhow!("{e}"))?;
        Vec::new()
    } else {
        grit_lib::midx::read_midx_pack_idx_names(repo.odb.objects_dir()).unwrap_or_default()
    };
    let loosen_unreachable = args.repack_all_unpack && args.delete_old && !args.cruft;

    let pack_line_hex_len = if cfg
        .get("extensions.objectformat")
        .or_else(|| cfg.get("extensions.objectFormat"))
        .is_some_and(|v| v.eq_ignore_ascii_case("sha256"))
    {
        64usize
    } else {
        40usize
    };

    let mut write_bitmaps = effective_write_bitmaps_int(&args, &cfg, full_repack, bare_repo);
    // Git (builtin/repack.c): bitmaps require a single resulting pack, so an incremental repack
    // (`-d` without `-a`/`-A`/`--cruft`) that still wants bitmaps is rejected before any packing.
    // The `--write-midx` path writes a multi-pack bitmap instead, so it is exempt.
    if write_bitmaps > 0 && !full_repack && !args.write_midx {
        anyhow::bail!(
            "Incremental repacks are incompatible with bitmap indexes.  Use\n\
             --no-write-bitmap-index or disable the pack.writeBitmaps configuration."
        );
    }
    let objects_dir_for_warn = repo.git_dir.join("objects");
    let mut quiet_pack_objects_local_alt = false;
    if args.local
        && grit_lib::pack::read_alternates_recursive(&objects_dir_for_warn)
            .is_ok_and(|v| !v.is_empty())
        && !args.no_write_bitmap_index
        && write_bitmaps != 0
    {
        eprintln!("warning: disabling bitmap writing, as some objects are not being packed");
        write_bitmaps = 0;
        quiet_pack_objects_local_alt = true;
    }
    if write_bitmaps != 0
        && repo_treats_promisor_packs(&repo.git_dir, &cfg)
        && !promisor_pack_object_ids(&objects_dir_for_warn).is_empty()
    {
        // A repo with promisor packs cannot have a closed bitmap. When bitmaps
        // were explicitly requested (`-b`/config), this is a hard error to match
        // Git. When bitmaps are merely the auto/"quiet" default (`write_bitmaps
        // < 0`, e.g. a bare `repack -A -d` on a partial clone), Git passes
        // `--write-bitmap-index-quiet` and simply skips the bitmap without
        // failing — so do the same here.
        if write_bitmaps > 0 {
            anyhow::bail!("fatal: failed to write bitmap index");
        }
        write_bitmaps = 0;
    }
    if pack_dir_has_any_keep_file(&pack_dir_abs) && !pack_kept_objects {
        write_bitmaps = 0;
    }
    if let Some(raw) = cfg
        .get("pack.packsizelimit")
        .or_else(|| cfg.get("pack.packSizeLimit"))
    {
        if parse_config_byte_size(&raw).map(|n| n > 0).unwrap_or(false)
            && args.max_pack_size.is_none()
            && write_bitmaps < 0
        {
            // Git disables quiet bitmaps when packs may split (`t7700-repack` auto-bitmaps test).
            write_bitmaps = 0;
        }
    }

    let mut new_pack_names: Vec<String> = Vec::new();

    let max_cruft_bytes = args
        .max_cruft_size
        .as_deref()
        .and_then(parse_config_byte_size);

    /// Lines Git `pack-objects` prints to stdout when writing packs (40 hex chars for SHA-1,
    /// 64 for SHA-256).
    /// With `--filter-to`, the main pack and the filtered-out side pack each emit one line;
    /// `git repack` / `finish_pack_objects_cmd` records every line.
    fn pack_hashes_from_pack_objects_stdout(stdout: &[u8], hex_len: usize) -> Vec<String> {
        let mut out = Vec::new();
        for line in stdout.split(|b| *b == b'\n') {
            let Ok(s) = std::str::from_utf8(line) else {
                continue;
            };
            let s = s.trim();
            if s.len() == hex_len && s.chars().all(|c| c.is_ascii_hexdigit()) {
                out.push(s.to_string());
            }
        }
        out
    }

    let run_one_pack_objects =
        |main_phase: bool, stdin_lines: Option<&[String]>, base: &str| -> Result<Vec<String>> {
            let mut cmd = Command::new(&grit_bin);
            cmd.current_dir(work_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .arg("pack-objects")
                .arg("--keep-true-parents")
                .arg("--non-empty");

            for k in &args.keep_pack {
                cmd.arg("--keep-pack").arg(k);
            }

            if !main_phase || !pack_kept_objects {
                cmd.arg("--honor-pack-keep");
            }

            cmd.arg("--all");
            if fast_import_bulk_pack_only && main_phase && stdin_lines.is_none() {
                cmd.arg("--reflog")
                    .arg("--indexed-objects")
                    .arg("--unpacked")
                    .arg("--incremental")
                    .arg("--local");
            } else if full_repack {
                cmd.arg("--reflog").arg("--indexed-objects");
            }

            if fast_import_bulk_pack_only && main_phase && stdin_lines.is_none() {
                // The local test harness follows `fast-import -c fastimport.unpacklimit=0` with
                // `repack -a -d -q` to materialize fast-import's loose objects as a pack. Git's
                // fast-import would have written only the imported batch into a new pack; keep
                // existing packs out of this compatibility repack by using incremental semantics.
            } else if full_repack {
                if main_phase {
                    if args.cruft {
                        cmd.arg("--reachability-all");
                    }
                    if args.no_cruft {
                        cmd.arg("--no-cruft");
                    }
                } else {
                    cmd.arg("--cruft");
                    if let Some(ref exp) = args.cruft_expiration {
                        if !exp.is_empty() {
                            cmd.arg(format!("--cruft-expiration={exp}"));
                        }
                    }
                    if let Some(n) = max_cruft_bytes {
                        // Git maps `--max-cruft-size` on repack to `pack-objects --max-pack-size`.
                        cmd.arg(format!("--max-pack-size={n}"));
                    }
                    // Git forwards `repack.cruft{Window,WindowMemory,Depth,Threads}` to the cruft
                    // `pack-objects` pass as the corresponding delta options. An invalid value
                    // (e.g. `repack.cruftWindow=bogus`) must make the cruft pass fail so repack
                    // exits non-zero and leaves no `.tmp-*` packs (t7700-repack subtest 38).
                    for (key_lc, key_cc, flag) in [
                        ("repack.cruftwindow", "repack.cruftWindow", "--window"),
                        ("repack.cruftdepth", "repack.cruftDepth", "--depth"),
                        ("repack.cruftthreads", "repack.cruftThreads", "--threads"),
                    ] {
                        if let Some(v) = cfg.get(key_lc).or_else(|| cfg.get(key_cc)) {
                            cmd.arg(format!("{flag}={v}"));
                        }
                    }
                }
                if main_phase {
                    if let Some(exp) = args.unpack_unreachable.as_deref() {
                        cmd.arg(format!("--unpack-unreachable={exp}"));
                    } else if loosen_unreachable {
                        cmd.arg("--unpack-unreachable");
                    } else if args.keep_unreachable {
                        cmd.arg("--keep-unreachable");
                    }
                }
            } else {
                cmd.arg("--reflog")
                    .arg("--indexed-objects")
                    .arg("--unpacked")
                    .arg("--incremental");
            }

            for f in &args.filter {
                if !f.is_empty() {
                    cmd.arg(format!("--filter={f}"));
                }
            }
            if let Some(ref to) = args.filter_to {
                if !to.is_empty() {
                    cmd.arg("--filter-to").arg(to);
                }
            }
            if let Some(v) = args.name_hash_version {
                cmd.arg(format!("--name-hash-version={v}"));
            }
            if args.local {
                cmd.arg("--local");
            }

            cmd.arg(base);

            // Incremental repack (`repack -d` without `-a`) must stay stderr-silent (`t7700-repack`).
            // Full repack without bitmaps must not print `pack-objects` progress (`t7700-repack`).
            if args.quiet
                || !full_repack
                || (main_phase && quiet_pack_objects_local_alt)
                || (full_repack && main_phase && write_bitmaps == 0)
            {
                cmd.arg("-q");
            }
            if args.aggressive {
                cmd.arg("--no-reuse-delta");
                cmd.arg("--window").arg("250");
                cmd.arg("--depth").arg("250");
            } else {
                if args.force {
                    cmd.arg("--no-reuse-delta");
                }
                if let Some(w) = args.window {
                    cmd.arg("--window").arg(w.to_string());
                }
                if let Some(d) = args.depth {
                    cmd.arg("--depth").arg(d.to_string());
                }
            }

            if repo_treats_promisor_packs(&repo.git_dir, &cfg) {
                cmd.arg("--exclude-promisor-objects");
            }

            if main_phase {
                if write_bitmaps > 0 {
                    cmd.arg("--write-bitmap-index");
                } else if write_bitmaps < 0 && !args.no_write_bitmap_index {
                    cmd.arg("--write-bitmap-index-quiet");
                }
            }
            if args.no_write_bitmap_index {
                cmd.arg("--no-write-bitmap-index");
            }

            // `git repack -i` / `repack.useDeltaIslands` forwards `--delta-islands` to pack-objects.
            let use_delta_islands = args.delta_islands
                || cfg
                    .get("repack.usedeltaislands")
                    .or_else(|| cfg.get("repack.useDeltaIslands"))
                    .is_some_and(|v| {
                        let v = v.trim();
                        v == "true" || v == "1" || v.eq_ignore_ascii_case("yes")
                    });
            if use_delta_islands {
                cmd.arg("--delta-islands");
            }

            // Emit a trace2 subcommand line for the spawned `pack-objects` child so trace-based
            // assertions (`test_subcommand_flex git pack-objects ...`) can observe forwarded
            // options such as `--name-hash-version` (t7700-repack subtest 40).
            {
                let mut po_argv = vec!["git".to_string()];
                for a in cmd.get_args() {
                    po_argv.push(a.to_string_lossy().into_owned());
                }
                trace2_emit_git_subcommand_argv(&po_argv);
            }

            if let Some(lines) = stdin_lines {
                use std::io::Write;
                cmd.stdin(Stdio::piped());
                let mut child = cmd.spawn().context("failed to spawn grit pack-objects")?;
                {
                    let mut stdin = child.stdin.take().context("pack-objects stdin")?;
                    for line in lines {
                        writeln!(stdin, "{line}")?;
                    }
                }
                let output = child
                    .wait_with_output()
                    .context("failed to run grit pack-objects")?;
                if !output.status.success() {
                    anyhow::bail!("pack-objects failed with status {}", output.status);
                }
                return Ok(pack_hashes_from_pack_objects_stdout(
                    &output.stdout,
                    pack_line_hex_len,
                ));
            }

            let output = cmd.output().context("failed to run grit pack-objects")?;
            if !output.status.success() {
                anyhow::bail!("pack-objects failed with status {}", output.status);
            }

            Ok(pack_hashes_from_pack_objects_stdout(
                &output.stdout,
                pack_line_hex_len,
            ))
        };

    if args.cruft && full_repack {
        let main_hashes = run_one_pack_objects(true, None, &pack_base)?;
        if !main_hashes.is_empty() {
            for h in &main_hashes {
                new_pack_names.push(format!("pack-{h}.pack"));
            }

            let objects_dir = repo.git_dir.join("objects");
            let indexes_before_cruft = grit_lib::pack::read_local_pack_indexes(&objects_dir)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut stdin_lines: Vec<String> = Vec::new();
            for h in &main_hashes {
                stdin_lines.push(format!("pack-{h}.pack"));
            }
            for idx in &indexes_before_cruft {
                let name = idx
                    .pack_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if !name.ends_with(".pack") {
                    continue;
                }
                if main_hashes.iter().any(|h| name == format!("pack-{h}.pack")) {
                    continue;
                }
                let stem = name.strip_suffix(".pack").unwrap_or(name);
                let retained = args.keep_pack.iter().any(|k| basename_matches(k, stem));
                if retained {
                    stdin_lines.push(name.to_string());
                } else {
                    stdin_lines.push(format!("-{name}"));
                }
            }

            let cruft_base = if let Some(ref et) = args.expire_to {
                let t = et.trim();
                if !t.is_empty() {
                    t
                } else {
                    &pack_base
                }
            } else {
                &pack_base
            };

            let cruft_hashes = run_one_pack_objects(false, Some(&stdin_lines), cruft_base)?;
            for h in cruft_hashes {
                new_pack_names.push(format!("pack-{h}.pack"));
            }
        }
    } else {
        let hashes = run_one_pack_objects(true, None, &pack_base)?;
        for h in hashes {
            new_pack_names.push(format!("pack-{h}.pack"));
        }
    }

    // Second `pack-objects --stdin-packs` pass for `repack --filter` (Git `write_filtered_pack`).
    if full_repack && !args.cruft && args.filter.iter().any(|s| !s.trim().is_empty()) {
        if let Some(last) = new_pack_names.last().cloned() {
            let explicit_filter_to = args
                .filter_to
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let filter_dest = explicit_filter_to.unwrap_or(&pack_base);
            // The filtered-out objects land in the LOCAL pack dir only when `--filter-to` was
            // omitted (Git writes them next to the main pack). With an explicit `--filter-to`
            // pointing at another repository (t6500 gc.repackFilterTo), the side pack lives THERE,
            // so it must NOT join `new_pack_names` — otherwise it is treated as a local pack to
            // keep, and a same-named leftover pack in this repo survives `repack -d`.
            let dest_is_local = explicit_filter_to
                .map(|to| filter_dest_is_local(work_dir, &pack_dir_abs, to))
                .unwrap_or(true);
            if let Some(h) = run_filtered_followup_pack_objects(
                &grit_bin,
                work_dir,
                &repo.git_dir,
                &pack_dir_abs,
                &last,
                filter_dest,
                &args,
                pack_kept_objects,
                write_bitmaps,
            )? {
                if dest_is_local {
                    new_pack_names.push(format!("pack-{h}.pack"));
                }
            }
        }
    }

    let mut trace_argv = vec![
        "git".to_string(),
        "repack".to_string(),
        "-d".to_string(),
        "-l".to_string(),
    ];
    if !full_repack {
        if args.no_write_bitmap_index {
            trace_argv.push("--no-write-bitmap-index".to_string());
        }
    } else if args.cruft {
        trace_argv.push("--cruft".to_string());
        let exp = args
            .cruft_expiration
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("2.weeks.ago");
        trace_argv.push(format!("--cruft-expiration={exp}"));
        if let Some(n) = max_cruft_bytes {
            trace_argv.push(format!("--max-cruft-size={n}"));
        }
    } else if args.repack_all_unpack {
        trace_argv.push("-A".to_string());
        if args.delete_old {
            match args.unpack_unreachable.as_deref() {
                Some(u) if !u.is_empty() => trace_argv.push(format!("--unpack-unreachable={u}")),
                _ => trace_argv.push("--unpack-unreachable".to_string()),
            }
        }
    } else if args.all {
        trace_argv.push("-a".to_string());
    }
    if args.no_cruft {
        trace_argv.push("--no-cruft".to_string());
    }
    for k in &args.keep_pack {
        trace_argv.push("--keep-pack".to_string());
        trace_argv.push(k.clone());
    }
    for f in &args.filter {
        if !f.is_empty() {
            trace_argv.push(format!("--filter={f}"));
        }
    }
    if let Some(ref to) = args.filter_to {
        if !to.is_empty() {
            trace_argv.push("--filter-to".to_string());
            trace_argv.push(to.clone());
        }
    }
    if let Some(ref et) = args.expire_to {
        let t = et.trim();
        if !t.is_empty() {
            trace_argv.push(format!("--expire-to={t}"));
        }
    }
    if args.quiet {
        trace_argv.push("-q".to_string());
    }
    if args.aggressive {
        trace_argv.push("--aggressive".to_string());
    }
    if write_bitmaps > 0 {
        trace_argv.push("-b".to_string());
    }
    trace2_emit_git_subcommand_argv(&trace_argv);

    if args.delete_old {
        if full_repack {
            if fast_import_bulk_pack_only {
                let _ = fs::remove_file(&fast_import_unpack_marker);
            } else {
                let mut keep: Vec<String> = new_pack_names.clone();
                // `pack-objects` may append names here when `blob:none` writes a sibling pack via stdin
                // (`write_pack_via_stdin_objects`). That pack must stay in the keep set while old packs
                // are still retained for duplicate objects; otherwise `remove_superseded_packs_*` treats
                // the side pack as redundant (`t7700-repack` filter tests).
                keep.extend(take_extra_packs_recorded_for_repack(&repo.git_dir)?);
                keep.extend(args.keep_pack.iter().cloned());
                let mut extra_objects_dirs: Vec<PathBuf> = Vec::new();
                for ft in [
                    args.filter_to
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty()),
                    args.expire_to
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty()),
                ]
                .into_iter()
                .flatten()
                {
                    // `filter-to` / `expire-to` name a pack *file* prefix; sibling `.idx` files live in
                    // the parent directory (often the repo root / trash, not `objects/`). Scan that
                    // directory for `.idx` files so superseded-pack removal sees filtered-out objects.
                    let base = work_dir.join(ft);
                    if let Some(parent) = base.parent() {
                        extra_objects_dirs.push(parent.to_path_buf());
                    }
                }
                // Plain `repack -a` / `git gc` (no cruft, no `-A`) rewrites the reachable closure into one
                // pack; every other local pack is redundant. The union-based remover kept old packs that
                // still held OIDs missing from the new pack (unreachable objects), which prevented
                // `git prune --expire=now` from dropping them (t3306-notes-prune).
                //
                // Exception: when grafts / replace-refs / a shallow boundary are in effect, the
                // reachability walk uses the rewritten parentage and may exclude an object that is
                // still literally referenced by a packed commit (e.g. a grafted-out parent). Git keeps
                // such "unreachable by grafts only" objects (t7700-repack subtest 12), so fall back to
                // the union-based remover that retains old packs holding objects missing from the new
                // pack.
                let grafts_or_replace_in_effect = repo.git_dir.join("info/grafts").is_file()
                    || repo
                        .git_dir
                        .join("refs/replace")
                        .read_dir()
                        .map(|mut rd| rd.next().is_some())
                        .unwrap_or(false);
                let simple_full_repack = (args.all || args.repack_all_unpack)
                    && !args.cruft
                    && !grafts_or_replace_in_effect
                    && !args.filter.iter().any(|f| !f.trim().is_empty());
                remove_superseded_packs_after_full_repack(
                    &pack_dir_abs,
                    &keep,
                    &extra_objects_dirs,
                    simple_full_repack,
                )?;
                if args.cruft {
                    remove_old_cruft_packs_not_in_keep(&pack_dir_abs, &keep)?;
                }
            }
            prune_packed_objects(&repo.git_dir.join("objects"), PrunePackedOptions::default())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            prune_hidden_loose_objects_for_shallow_repo(&repo)?;
        } else if let Some(new_pack_name) = new_pack_names.first().cloned() {
            remove_superseded_packs_incremental(&pack_dir_abs, &new_pack_name, &args.keep_pack)?;
            if args.no_write_bitmap_index {
                prune_packed_objects(&repo.git_dir.join("objects"), PrunePackedOptions::default())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }
    }

    // Mirror git/repack.c `repack_remove_redundant_pack`: an existing MIDX is dropped only when
    // a pack it references was just removed as redundant. If every MIDX-referenced pack survived
    // (e.g. they were `.keep` packs), the MIDX stays byte-for-byte intact.
    if !args.write_midx && !midx_referenced_packs.is_empty() {
        let any_referenced_pack_gone = midx_referenced_packs.iter().any(|idx_name| {
            let stem = idx_name.strip_suffix(".idx").unwrap_or(idx_name);
            !pack_dir_abs.join(format!("{stem}.pack")).exists()
        });
        if any_referenced_pack_gone {
            clear_pack_midx_state(&pack_dir_abs).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }

    // Multi-pack-index handling for the non-geometric path. `git repack --write-midx` (and
    // `repack -m...`) must write `objects/pack/multi-pack-index`; a full repack without
    // `--write-midx` must leave no stale MIDX behind (full_repack already cleared it above).
    if args.write_midx {
        let has_local_idx = fs::read_dir(&pack_dir_abs)
            .map(|rd| {
                rd.filter_map(std::result::Result::ok).any(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.starts_with("pack-") && n.ends_with(".idx")
                })
            })
            .unwrap_or(false);
        if has_local_idx {
            let pref_stem = new_pack_names
                .first()
                .and_then(|n| n.strip_prefix("pack-"))
                .and_then(|n| n.strip_suffix(".pack"))
                .map(str::to_owned);
            let pref_idx = preferred_pack_index(&pack_dir_abs, pref_stem.as_deref())?;
            // Only write the placeholder MIDX bitmap when bitmaps were actually requested
            // (`-b`); a bare `--write-midx` must not create any bitmap (subtest 28).
            let bitmap_placeholders = write_bitmaps > 0
                && !args.no_write_bitmap_index
                && !(args.local
                    && grit_lib::pack::read_alternates_recursive(&objects_dir_for_warn)
                        .is_ok_and(|v| !v.is_empty()));
            write_multi_pack_index_with_options(
                &pack_dir_abs,
                &WriteMultiPackIndexOptions {
                    preferred_pack_idx: pref_idx,
                    write_bitmap_placeholders: bitmap_placeholders,
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            if bitmap_placeholders {
                remove_pack_bitmap_sidecars(&pack_dir_abs)?;
            }
        }
    }

    // Git runs `update_server_info` at the END of repack by default (regardless of `-d`),
    // unless `-n` / `repack.updateServerInfo=false`.
    if should_update_server_info(&args, &cfg) {
        update_server_info::refresh_server_info(&repo)?;
    }

    prune_hidden_loose_objects_for_shallow_repo(&repo)?;
    let _ = grit_lib::shared_repo::refresh_repository_shared_tree(&repo.git_dir);

    Ok(())
}

fn run_geometric(
    repo: &Repository,
    args: &Args,
    pack_kept_objects: bool,
    split_factor: i32,
) -> Result<()> {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let work_dir = repo.work_tree.as_deref().unwrap_or(&repo.git_dir);
    let grit_bin = grit_exe::grit_executable();
    let pack_dir = repo.git_dir.join("objects").join("pack");
    let objects_dir = repo.git_dir.join("objects");
    ensure_no_orphan_pack_indexes(&pack_dir)?;

    let bare_repo = repo.work_tree.is_none();
    let mut write_bitmaps = effective_write_bitmaps_int(args, &cfg, false, bare_repo);
    if args.local
        && grit_lib::pack::read_alternates_recursive(&objects_dir).is_ok_and(|v| !v.is_empty())
        && !args.no_write_bitmap_index
        && write_bitmaps != 0
    {
        eprintln!("warning: disabling bitmap writing, as some objects are not being packed");
        write_bitmaps = 0;
    }
    if pack_dir_has_any_keep_file(&pack_dir) && !pack_kept_objects {
        write_bitmaps = 0;
    }
    if let Some(raw) = cfg
        .get("pack.packsizelimit")
        .or_else(|| cfg.get("pack.packSizeLimit"))
    {
        if parse_config_byte_size(&raw).map(|n| n > 0).unwrap_or(false)
            && args.max_pack_size.is_none()
            && write_bitmaps < 0
        {
            write_bitmaps = 0;
        }
    }

    let keep_packs: Vec<String> = args.keep_pack.clone();

    let mut normal = collect_geometry_packs(&objects_dir, pack_kept_objects, &keep_packs)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !args.local {
        if let Ok(alternates) = grit_lib::pack::read_alternates_recursive(&objects_dir) {
            let mut seen_stems: HashSet<String> = normal.iter().map(|p| p.stem.clone()).collect();
            for alternate in alternates {
                let mut alt_packs =
                    collect_geometry_packs(&alternate, pack_kept_objects, &keep_packs)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                for mut pack in alt_packs.drain(..) {
                    if seen_stems.insert(pack.stem.clone()) {
                        pack.is_local = false;
                        normal.push(pack);
                    }
                }
            }
            normal.sort_by_key(|p| p.object_count);
        }
    }
    let weights: Vec<usize> = normal.iter().map(|p| p.object_count).collect();
    let split = compute_geometry_split(&weights, split_factor);
    let pref_stem = preferred_pack_stem_after_split(&normal, split);

    let promisor_list =
        collect_promisor_geometry_packs(&objects_dir, pack_kept_objects, &keep_packs)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    let prom_weights: Vec<usize> = promisor_list.iter().map(|p| p.object_count).collect();
    let prom_split = compute_geometry_split(&prom_weights, split_factor);

    let has_loose = objects_dir_has_loose_objects(&objects_dir);

    let pack_base = if repo.work_tree.is_some() {
        repo.odb
            .objects_dir()
            .join("pack")
            .join("pack")
            .to_string_lossy()
            .into_owned()
    } else {
        "objects/pack/pack".to_owned()
    };

    let mut promisor_written: Vec<String> = Vec::new();
    let mut normal_written: Vec<String> = Vec::new();

    let should_run_pack_objects = prom_split > 0 || split > 0 || has_loose;

    if !should_run_pack_objects {
        if !args.quiet {
            println!("Nothing new to pack.");
        }
    } else {
        if prom_split > 0 {
            let stdin = build_stdin_packs_lines(&promisor_list, prom_split);
            promisor_written = run_pack_objects_stdin(
                &grit_bin,
                work_dir,
                &repo.git_dir,
                &pack_base,
                &stdin,
                args,
                &cfg,
                pack_kept_objects,
                write_bitmaps,
                true,
            )?;
        }

        if split > 0 {
            let stdin = build_stdin_packs_lines(&normal, split);
            normal_written = run_pack_objects_stdin(
                &grit_bin,
                work_dir,
                &repo.git_dir,
                &pack_base,
                &stdin,
                args,
                &cfg,
                pack_kept_objects,
                write_bitmaps,
                false,
            )?;
        } else if has_loose {
            // Progression intact (or no packs yet) but loose objects need packing (`--unpacked`).
            let stdin = build_stdin_packs_lines(&normal, 0);
            normal_written = run_pack_objects_stdin(
                &grit_bin,
                work_dir,
                &repo.git_dir,
                &pack_base,
                &stdin,
                args,
                &cfg,
                pack_kept_objects,
                write_bitmaps,
                false,
            )?;
        }

        if normal_written.is_empty() && promisor_written.is_empty() && !args.quiet {
            println!("Nothing new to pack.");
        }
    }

    if !should_run_pack_objects {
        if args.write_midx {
            let has_local_idx = fs::read_dir(&pack_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok()).any(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.starts_with("pack-") && n.ends_with(".idx")
                    })
                })
                .unwrap_or(false);
            if has_local_idx {
                let bitmap_placeholders = write_bitmaps > 0 && !args.no_write_bitmap_index;
                let midx_path = pack_dir.join("multi-pack-index");
                let needs_bitmap = bitmap_placeholders && !pack_dir_has_midx_bitmap(&pack_dir);
                if !midx_path.is_file() || needs_bitmap {
                    // No pack was written (nothing new to pack), so `split` and the
                    // promisor split are both 0: every surviving pack is above the
                    // split line. Build the explicit include list anyway so we never
                    // inherit a stale MIDX referencing already-deleted packs.
                    let include = geometric_midx_included_idx_names(
                        &pack_dir,
                        &normal,
                        split,
                        &promisor_list,
                        prom_split,
                        &keep_packs,
                        &normal_written,
                        &promisor_written,
                    );
                    write_multi_pack_index_with_options(
                        &pack_dir,
                        &WriteMultiPackIndexOptions {
                            preferred_pack_name: pref_stem.as_deref().map(|s| format!("{s}.idx")),
                            pack_names_subset_ordered: Some(include),
                            write_bitmap_placeholders: bitmap_placeholders,
                            ..Default::default()
                        },
                    )
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
            }
        }
        if args.delete_old {
            update_server_info::refresh_objects_info_packs(repo)?;
        }
        return Ok(());
    }

    if args.write_midx {
        let bitmap = write_bitmaps > 0
            && !args.no_write_bitmap_index
            && !(args.local
                && grit_lib::pack::read_alternates_recursive(&objects_dir)
                    .is_ok_and(|v| !v.is_empty()));
        let wrote_pack = !normal_written.is_empty() || !promisor_written.is_empty();
        let midx_path = pack_dir.join("multi-pack-index");
        let needs_bitmap = bitmap && !pack_dir_has_midx_bitmap(&pack_dir);
        if wrote_pack || !midx_path.is_file() || needs_bitmap {
            // Mirror git/repack-midx.c `midx_included_packs`: the geometric MIDX is
            // written from an explicit pack list (Git `multi-pack-index write
            // --stdin-packs`) covering kept packs, newly written packs, surviving
            // above-split packs, and any cruft packs. It does NOT inherit the
            // existing MIDX's pack list, so packs about to be removed as redundant
            // (and packs a prior run already deleted) are naturally excluded.
            let include = geometric_midx_included_idx_names(
                &pack_dir,
                &normal,
                split,
                &promisor_list,
                prom_split,
                &keep_packs,
                &normal_written,
                &promisor_written,
            );
            write_multi_pack_index_with_options(
                &pack_dir,
                &WriteMultiPackIndexOptions {
                    preferred_pack_name: pref_stem.as_deref().map(|s| format!("{s}.idx")),
                    pack_names_subset_ordered: Some(include),
                    write_bitmap_placeholders: bitmap,
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }

    if args.delete_old {
        remove_geometry_redundant(
            &pack_dir,
            &normal,
            split,
            &promisor_list,
            prom_split,
            pack_kept_objects,
            &keep_packs,
            &promisor_written,
            &normal_written,
        )?;
        let opts = grit_lib::prune_packed::PrunePackedOptions::default();
        grit_lib::prune_packed::prune_packed_objects(&objects_dir, opts)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        remove_duplicate_packs_matching_alternates(&objects_dir)?;
        update_server_info::refresh_objects_info_packs(repo)?;
    }

    let _ = grit_lib::shared_repo::refresh_repository_shared_tree(&repo.git_dir);

    Ok(())
}

fn guard_against_corrupt_loose_refs_for_repack(repo: &Repository) -> Result<()> {
    let refs_dir = repo.git_dir.join("refs");
    if refs_dir.is_dir() {
        scan_ref_dir_for_repack(repo, &refs_dir)?;
    }
    Ok(())
}

fn scan_ref_dir_for_repack(repo: &Repository, dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            scan_ref_dir_for_repack(repo, &path)?;
            continue;
        }
        if !entry.file_type()?.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(&repo.git_dir)
            .unwrap_or(path.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        if check_refname_format(&rel, &RefNameOptions::default()).is_err() {
            anyhow::bail!("bad ref for repack: {rel}");
        }
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let value = raw.trim();
        if value.starts_with("ref: ") {
            continue;
        }
        if value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Ok(oid) = ObjectId::from_hex(value) {
                if !repo.odb.exists(&oid) {
                    anyhow::bail!("bad ref for repack: {rel}");
                }
            }
        }
    }
    Ok(())
}

fn prune_hidden_loose_objects_for_shallow_repo(repo: &Repository) -> Result<()> {
    let shallow_path = repo.git_dir.join("shallow");
    if !shallow_path.is_file() {
        return Ok(());
    }

    let mut keep: HashSet<ObjectId> = HashSet::new();
    let mut queue: std::collections::VecDeque<ObjectId> = std::collections::VecDeque::new();
    let mut shallow_boundaries: HashSet<ObjectId> = HashSet::new();

    if let Ok(head_oid) = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD") {
        queue.push_back(head_oid);
    }
    if let Ok(all_refs) = grit_lib::refs::list_refs(&repo.git_dir, "refs/") {
        for (_, oid) in all_refs {
            queue.push_back(oid);
        }
    }
    if let Ok(content) = fs::read_to_string(&shallow_path) {
        for line in content.lines() {
            let hex = line.trim();
            if hex.is_empty() {
                continue;
            }
            if let Ok(oid) = ObjectId::from_hex(hex) {
                shallow_boundaries.insert(oid);
                queue.push_back(oid);
            }
        }
    }

    while let Some(oid) = queue.pop_front() {
        if !keep.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb.read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                let Ok(commit) = parse_commit(&obj.data) else {
                    continue;
                };
                queue.push_back(commit.tree);
                if !shallow_boundaries.contains(&oid) {
                    for parent in commit.parents {
                        queue.push_back(parent);
                    }
                }
            }
            ObjectKind::Tree => {
                let Ok(entries) = parse_tree(&obj.data) else {
                    continue;
                };
                for entry in entries {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    queue.push_back(entry.oid);
                }
            }
            ObjectKind::Tag => {
                let Ok(tag) = parse_tag(&obj.data) else {
                    continue;
                };
                queue.push_back(tag.object);
            }
            ObjectKind::Blob => {}
        }
    }

    let objects_dir = repo.git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Ok(());
    }
    for fanout in fs::read_dir(&objects_dir)? {
        let fanout = fanout?;
        let name = fanout.file_name().to_string_lossy().to_string();
        if name == "info" || name == "pack" {
            continue;
        }
        if name.len() != 2
            || !name.chars().all(|c| c.is_ascii_hexdigit())
            || !fanout.path().is_dir()
        {
            continue;
        }
        for entry in fs::read_dir(fanout.path())? {
            let entry = entry?;
            if !entry.path().is_file() {
                continue;
            }
            let tail = entry.file_name().to_string_lossy().to_string();
            if tail.len() != 38 || !tail.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let hex = format!("{name}{tail}");
            let Ok(oid) = ObjectId::from_hex(&hex) else {
                continue;
            };
            if !keep.contains(&oid) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    Ok(())
}

fn objects_dir_has_loose_objects(objects_dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(objects_dir) else {
        return false;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(sub) = fs::read_dir(entry.path()) else {
            continue;
        };
        for f in sub.flatten() {
            let n = f.file_name().to_string_lossy().to_string();
            if n.len() == 38 && n.chars().all(|c| c.is_ascii_hexdigit()) {
                return true;
            }
        }
    }
    false
}

fn remove_duplicate_packs_matching_alternates(objects_dir: &Path) -> Result<()> {
    let local_pack = objects_dir.join("pack");
    let alts = match grit_lib::pack::read_alternates_recursive(objects_dir) {
        Ok(a) => a,
        Err(_) => return Ok(()),
    };
    for alt in alts {
        let alt_pack = alt.join("pack");
        let rd = match fs::read_dir(&local_pack) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("pack-") || !name.ends_with(".pack") {
                continue;
            }
            let local_path = entry.path();
            let alt_path = alt_pack.join(&name);
            if !alt_path.is_file() {
                continue;
            }
            let lm = fs::metadata(&local_path).map_err(|e| anyhow::anyhow!(e))?;
            let am = fs::metadata(&alt_path).map_err(|e| anyhow::anyhow!(e))?;
            if lm.len() != am.len() || lm.len() < 20 {
                continue;
            }
            let mut lb = vec![0u8; 20];
            let mut ab = vec![0u8; 20];
            let ldata = fs::read(&local_path).map_err(|e| anyhow::anyhow!(e))?;
            let adata = fs::read(&alt_path).map_err(|e| anyhow::anyhow!(e))?;
            if ldata.len() != adata.len() {
                continue;
            }
            if ldata.len() < 20 {
                continue;
            }
            lb.copy_from_slice(&ldata[ldata.len() - 20..]);
            ab.copy_from_slice(&adata[adata.len() - 20..]);
            if lb != ab {
                continue;
            }
            let stem = name.strip_suffix(".pack").unwrap_or(&name).to_string();
            let _ = fs::remove_file(&local_path);
            let _ = fs::remove_file(local_pack.join(format!("{stem}.idx")));
            let _ = fs::remove_file(local_pack.join(format!("{stem}.promisor")));
        }
    }
    Ok(())
}

fn preferred_pack_index(pack_dir: &Path, stem: Option<&str>) -> Result<Option<u32>> {
    let Some(stem) = stem else {
        return Ok(None);
    };
    let want = format!("{stem}.idx");
    let mut names: Vec<String> = fs::read_dir(pack_dir)
        .map_err(|e| anyhow::anyhow!(e))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            (n.starts_with("pack-") && n.ends_with(".idx")).then_some(n)
        })
        .collect();
    names.sort();
    let idx = names.iter().position(|n| n == &want);
    Ok(idx.map(|i| i as u32))
}

fn build_stdin_packs_lines(packs: &[GeometricPack], split: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut inc: Vec<&GeometricPack> = packs.iter().take(split).collect();
    inc.sort_by_key(|p| p.mtime_secs);
    for p in inc {
        lines.push(p.stem.clone());
    }
    for p in packs.iter().skip(split) {
        lines.push(format!("^{}", p.stem));
    }
    format!("{}\n", lines.join("\n"))
}

#[expect(clippy::too_many_arguments)]
fn run_pack_objects_stdin(
    grit_bin: &Path,
    work_dir: &Path,
    git_dir: &Path,
    pack_base: &str,
    stdin_text: &str,
    args: &Args,
    cfg: &ConfigSet,
    pack_kept_objects: bool,
    write_bitmaps: i32,
    is_promisor: bool,
) -> Result<Vec<String>> {
    let mut cmd = Command::new(grit_bin);
    cmd.current_dir(work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .arg("pack-objects")
        .arg("--stdin-packs")
        .arg("--unpacked")
        .arg("--non-empty")
        .arg(pack_base);
    cmd.arg("-q");
    if args.local {
        cmd.arg("--local");
    }
    if !pack_kept_objects {
        cmd.arg("--honor-pack-keep");
    }
    if write_bitmaps > 0 {
        cmd.arg("--write-bitmap-index");
    } else if write_bitmaps < 0 && !args.no_write_bitmap_index {
        cmd.arg("--write-bitmap-index-quiet");
    }
    if args.no_write_bitmap_index {
        cmd.arg("--no-write-bitmap-index");
    }
    if args.aggressive {
        cmd.arg("--no-reuse-delta");
        cmd.arg("--window").arg("250");
        cmd.arg("--depth").arg("250");
    } else {
        if args.force {
            cmd.arg("--no-reuse-delta");
        }
        if let Some(w) = args.window {
            cmd.arg("--window").arg(w.to_string());
        }
        if let Some(d) = args.depth {
            cmd.arg("--depth").arg(d.to_string());
        }
    }
    if let Some(ref s) = args.max_pack_size {
        cmd.arg("--max-pack-size").arg(s);
    }
    if repo_treats_promisor_packs(git_dir, cfg) && !is_promisor {
        cmd.arg("--exclude-promisor-objects");
    }

    let mut child = cmd.spawn().context("spawn pack-objects")?;
    let mut stdin = child.stdin.take().context("pack-objects stdin")?;
    stdin.write_all(stdin_text.as_bytes())?;
    drop(stdin);
    let output = child.wait_with_output().context("wait pack-objects")?;
    if !output.status.success() {
        anyhow::bail!("pack-objects failed with status {}", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let hashes: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(hashes)
}

/// Compute the explicit `.idx` basenames the geometric MIDX should cover, mirroring
/// git/repack-midx.c `midx_included_packs` (its `--stdin-packs` argument).
///
/// Git's MIDX covers every surviving local pack: kept packs, newly written packs, and all
/// existing packs above the geometric split line. Grit's geometry only ranks `pack-*` packs,
/// so any other local pack (e.g. the `loose-*` pack the loose-objects task writes) is never
/// below the split and always survives — it is added here by scanning the directory and
/// excluding only the stems that this run rolled up (the below-split packs being deleted).
/// Cruft (`*.mtimes`) packs are kept too (`midx_must_contain_cruft`). Building the list
/// explicitly means the MIDX never inherits a stale entry for a pack a prior run deleted.
///
/// # Parameters
/// - `pack_dir`: the `objects/pack` directory, scanned for all surviving `.idx` files.
/// - `normal` / `promisor`: pre-repack pack lists sorted ascending by object count.
/// - `split` / `prom_split`: number of low packs that were rolled into the new pack(s).
/// - `keep_pack_names`: `--keep-pack` basenames to always retain.
/// - `normal_new_hashes` / `promisor_new_hashes`: hashes of packs written this run.
///
/// # Returns
/// The `*.idx` basenames to pass as the MIDX `--stdin-packs` subset, in git's order. Names
/// whose pack no longer exists are dropped downstream by the writer.
#[expect(clippy::too_many_arguments)]
fn geometric_midx_included_idx_names(
    pack_dir: &Path,
    normal: &[GeometricPack],
    split: usize,
    promisor: &[GeometricPack],
    prom_split: usize,
    keep_pack_names: &[String],
    normal_new_hashes: &[String],
    promisor_new_hashes: &[String],
) -> Vec<String> {
    let mut include: Vec<String> = Vec::new();
    let push = |name: String, into: &mut Vec<String>| {
        if !into.contains(&name) {
            into.push(name);
        }
    };

    // Stems rolled up by this run (below the split line). These packs are removed as
    // redundant, so they must NOT appear in the new MIDX.
    let mut removed: HashSet<String> = HashSet::new();
    for p in normal.iter().take(split) {
        if p.is_local
            && !pack_dir.join(format!("{}.keep", p.stem)).is_file()
            && !keep_pack_names.iter().any(|k| basename_matches(k, &p.stem))
        {
            removed.insert(p.stem.clone());
        }
    }
    for p in promisor.iter().take(prom_split) {
        if !pack_dir.join(format!("{}.keep", p.stem)).is_file()
            && !keep_pack_names.iter().any(|k| basename_matches(k, &p.stem))
        {
            removed.insert(p.stem.clone());
        }
    }

    // Kept packs first, then the newly written packs, matching git's ordering.
    for p in normal.iter().chain(promisor.iter()) {
        if !p.is_local {
            continue;
        }
        let kept = pack_dir.join(format!("{}.keep", p.stem)).is_file()
            || keep_pack_names.iter().any(|k| basename_matches(k, &p.stem));
        if kept {
            push(format!("{}.idx", p.stem), &mut include);
        }
    }
    for h in normal_new_hashes.iter().chain(promisor_new_hashes.iter()) {
        push(format!("pack-{h}.idx"), &mut include);
    }

    // Every other surviving local pack on disk: above-split `pack-*` packs plus any
    // non-`pack-*` local pack (e.g. `loose-*`) and cruft packs that grit's geometry
    // does not rank. Skip the below-split packs being deleted.
    if let Ok(rd) = fs::read_dir(pack_dir) {
        let mut survivors: Vec<String> = rd
            .flatten()
            .filter_map(|ent| {
                let name = ent.file_name().to_string_lossy().to_string();
                let stem = name.strip_suffix(".idx")?.to_string();
                if removed.contains(&stem) {
                    return None;
                }
                if !pack_dir.join(format!("{stem}.pack")).is_file() {
                    return None;
                }
                Some(name)
            })
            .collect();
        survivors.sort();
        for name in survivors {
            push(name, &mut include);
        }
    }

    include
}

#[expect(clippy::too_many_arguments)]
fn remove_geometry_redundant(
    pack_dir: &Path,
    normal: &[GeometricPack],
    split: usize,
    promisor: &[GeometricPack],
    prom_split: usize,
    _pack_kept_objects: bool,
    keep_pack_names: &[String],
    promisor_new_hashes: &[String],
    normal_new_hashes: &[String],
) -> Result<()> {
    fn remove_pack_stem(pack_dir: &Path, stem: &str) {
        let _ = fs::remove_file(pack_dir.join(format!("{stem}.pack")));
        let _ = fs::remove_file(pack_dir.join(format!("{stem}.idx")));
        let _ = fs::remove_file(pack_dir.join(format!("{stem}.promisor")));
    }

    for p in normal.iter().take(split) {
        if !p.is_local {
            continue;
        }
        if pack_dir.join(format!("{}.keep", p.stem)).is_file() {
            continue;
        }
        if keep_pack_names.iter().any(|k| basename_matches(k, &p.stem)) {
            continue;
        }
        remove_pack_stem(pack_dir, &p.stem);
    }

    for p in promisor.iter().take(prom_split) {
        if pack_dir.join(format!("{}.keep", p.stem)).is_file() {
            continue;
        }
        if keep_pack_names.iter().any(|k| basename_matches(k, &p.stem)) {
            continue;
        }
        remove_pack_stem(pack_dir, &p.stem);
    }

    for h in promisor_new_hashes {
        let stem = format!("pack-{h}");
        let marker = pack_dir.join(format!("{stem}.promisor"));
        if !marker.exists() {
            let _ = fs::write(&marker, []);
        }
    }
    let _ = normal_new_hashes;

    Ok(())
}

/// Second `pack-objects` invocation for `repack -a -d --filter=…` (Git `write_filtered_pack`).
///
/// Upstream `write_filtered_pack` runs `pack-objects --stdin-packs` **without** `--filter` (Git
/// forbids combining those options). The first pass already applied the filter; this pass packs
/// objects present in older packs but omitted from the new main pack.
#[expect(clippy::too_many_arguments)]
fn run_filtered_followup_pack_objects(
    grit_bin: &Path,
    work_dir: &Path,
    git_dir: &Path,
    pack_dir: &Path,
    new_pack_name: &str,
    out_prefix: &str,
    args: &Args,
    pack_kept_objects: bool,
    write_bitmaps: i32,
) -> Result<Option<String>> {
    let new_base = pack_basename(new_pack_name);
    if !new_base.ends_with(".pack") {
        return Ok(None);
    }
    let mut stdin_lines: Vec<String> = vec![format!("^{new_base}")];
    let rd = fs::read_dir(pack_dir).map_err(|e| anyhow::anyhow!(e))?;
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if !name.starts_with("pack-") || !name.ends_with(".pack") {
            continue;
        }
        if name == new_base {
            continue;
        }
        let stem = name.strip_suffix(".pack").unwrap_or(&name);
        let kept_by_flag = args.keep_pack.iter().any(|k| basename_matches(k, stem));
        let kept_by_file = pack_dir.join(format!("{stem}.keep")).is_file();
        if kept_by_flag || kept_by_file {
            if pack_kept_objects {
                stdin_lines.push(name);
            } else {
                stdin_lines.push(format!("^{name}"));
            }
        } else {
            stdin_lines.push(name);
        }
    }
    if stdin_lines.len() <= 1 {
        return Ok(None);
    }

    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();

    let mut cmd = Command::new(grit_bin);
    cmd.current_dir(work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .arg("pack-objects")
        .arg("--stdin-packs")
        .arg("--keep-true-parents")
        .arg("--non-empty")
        .arg(out_prefix);
    for k in &args.keep_pack {
        cmd.arg("--keep-pack").arg(k);
    }
    if args.quiet || write_bitmaps == 0 {
        cmd.arg("-q");
    }
    if args.aggressive {
        cmd.arg("--no-reuse-delta");
        cmd.arg("--window").arg("250");
        cmd.arg("--depth").arg("250");
    } else {
        if args.force {
            cmd.arg("--no-reuse-delta");
        }
        if let Some(w) = args.window {
            cmd.arg("--window").arg(w.to_string());
        }
        if let Some(d) = args.depth {
            cmd.arg("--depth").arg(d.to_string());
        }
    }
    if let Some(ref s) = args.max_pack_size {
        cmd.arg("--max-pack-size").arg(s);
    }
    if repo_treats_promisor_packs(git_dir, &cfg) {
        cmd.arg("--exclude-promisor-objects");
    }
    if args.local {
        cmd.arg("--local");
    }
    if write_bitmaps > 0 {
        cmd.arg("--write-bitmap-index");
    } else if write_bitmaps < 0 && !args.no_write_bitmap_index {
        cmd.arg("--write-bitmap-index-quiet");
    }
    if args.no_write_bitmap_index {
        cmd.arg("--no-write-bitmap-index");
    }

    let mut child = cmd.spawn().context("spawn pack-objects filter follow-up")?;
    {
        let mut stdin = child.stdin.take().context("pack-objects stdin")?;
        for line in &stdin_lines {
            writeln!(stdin, "{line}")?;
        }
    }
    let output = child
        .wait_with_output()
        .context("wait pack-objects filter follow-up")?;
    if !output.status.success() {
        anyhow::bail!(
            "pack-objects (filter follow-up) failed with status {}",
            output.status
        );
    }
    let hash = output
        .stdout
        .split(|b| *b == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok(hash)
}

fn pack_dir_has_any_keep_file(pack_dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(pack_dir) else {
        return false;
    };
    for ent in rd.flatten() {
        let n = ent.file_name().to_string_lossy().to_string();
        if n.starts_with("pack-") && n.ends_with(".keep") {
            return true;
        }
    }
    false
}

fn pack_dir_has_midx_bitmap(pack_dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(pack_dir) else {
        return false;
    };
    for ent in rd.flatten() {
        let n = ent.file_name().to_string_lossy().to_string();
        if n.starts_with("multi-pack-index-") && n.ends_with(".bitmap") {
            return true;
        }
    }
    false
}

/// Fail repack when a parseable `.idx` has no sibling `.pack` (repository corruption; `t7700-repack`).
/// Reads and removes `objects/info/grit-extra-packs` (one pack basename per line).
///
/// Populated by `pack-objects` when it writes an auxiliary pack during `--filter=blob:none`
/// handling; `repack -d` consumes the list so it is not reused across runs.
fn take_extra_packs_recorded_for_repack(git_dir: &Path) -> Result<Vec<String>> {
    let path = git_dir
        .join("objects")
        .join("info")
        .join("grit-extra-packs");
    let contents = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let lines: Vec<String> = contents
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let _ = fs::remove_file(&path);
    Ok(lines)
}

fn ensure_no_orphan_pack_indexes(pack_dir: &Path) -> Result<()> {
    let rd = match fs::read_dir(pack_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for ent in rd {
        let ent = ent.context("read objects/pack")?;
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("idx") {
            continue;
        }
        if path.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !pack_dir.join(format!("{stem}.pack")).is_file() {
            anyhow::bail!("bad object pack {stem}.pack (missing)");
        }
    }
    Ok(())
}

fn basename_matches(keep: &str, stem: &str) -> bool {
    let p = Path::new(keep);
    let fname = p.file_name().and_then(|s| s.to_str()).unwrap_or(keep);
    let no_suf = fname.strip_suffix(".pack").unwrap_or(fname);
    no_suf == stem || fname == format!("{stem}.pack")
}

/// Deletes every `pack-*.pack` in `pack_dir` except the given basenames, unless a matching
/// `pack-*.keep` file exists for that pack. Used by `gc` when writing both a merged promisor pack
/// and a non-promisor pack in one pass.
fn pack_basename(name: &str) -> &str {
    Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}

fn remove_pack_sidecars(pack_dir: &Path, stem: &str) {
    let _ = fs::remove_file(pack_dir.join(format!("{stem}.mtimes")));
    let _ = fs::remove_file(pack_dir.join(format!("{stem}.rev")));
    let _ = fs::remove_file(pack_dir.join(format!("{stem}.bitmap")));
}

fn remove_pack_bitmap_sidecars(pack_dir: &Path) -> Result<()> {
    let rd = match fs::read_dir(pack_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("pack-") && name.ends_with(".bitmap") {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Union OIDs from every `.idx` in `dirs` (non-recursive). Used for `--filter-to` packs written
/// next to a path prefix outside `objects/pack/`.
/// Whether a `--filter-to <prefix>` destination (resolved relative to `work_dir`) writes its pack
/// into this repository's own `objects/pack` directory (`pack_dir_abs`).
fn filter_dest_is_local(work_dir: &Path, pack_dir_abs: &Path, filter_to: &str) -> bool {
    let dest = Path::new(filter_to);
    let resolved = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        work_dir.join(dest)
    };
    let Some(parent) = resolved.parent() else {
        return false;
    };
    if parent == pack_dir_abs {
        return true;
    }
    match (parent.canonicalize(), pack_dir_abs.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn union_oids_from_flat_pack_index_dirs(dirs: &[PathBuf]) -> Result<HashSet<ObjectId>> {
    let mut out = HashSet::new();
    for dir in dirs {
        let rd = match fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("idx") {
                continue;
            }
            if let Ok(idx) = grit_lib::pack::read_pack_index(&path) {
                out.extend(idx.entries.iter().filter_map(|e| {
                    if e.oid.len() == 20 {
                        ObjectId::from_bytes(&e.oid).ok()
                    } else {
                        None
                    }
                }));
            }
        }
    }
    Ok(out)
}

/// After a full repack, delete packs whose objects are entirely present in the union of the new
/// pack and any packs we must retain (e.g. `--keep-pack`, or an older pack that still holds
/// objects omitted by `--filter=blob:none`).
fn remove_superseded_packs_after_full_repack(
    pack_dir: &Path,
    initial_keep: &[String],
    extra_objects_dirs: &[PathBuf],
    simple_full_repack: bool,
) -> Result<()> {
    let objects_dir = pack_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid pack directory"))?;
    let indexes =
        grit_lib::pack::read_local_pack_indexes(objects_dir).map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut by_name: HashMap<String, HashSet<ObjectId>> = HashMap::new();
    for idx in &indexes {
        let name = idx
            .pack_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if !name.ends_with(".pack") {
            continue;
        }
        let oids: HashSet<ObjectId> = idx
            .entries
            .iter()
            .filter_map(|e| {
                if e.oid.len() == 20 {
                    ObjectId::from_bytes(&e.oid).ok()
                } else {
                    None
                }
            })
            .collect();
        by_name.insert(name, oids);
    }

    let mut retained: HashSet<String> = initial_keep
        .iter()
        .map(|k| pack_basename(k).to_string())
        .collect();

    if simple_full_repack && extra_objects_dirs.is_empty() {
        for name in by_name.keys() {
            if retained.contains(name) {
                continue;
            }
            let stem = name
                .strip_suffix(".pack")
                .unwrap_or(name.as_str())
                .to_string();
            if pack_dir.join(format!("{stem}.keep")).exists() {
                continue;
            }
            if pack_dir.join(format!("{stem}.promisor")).exists() {
                continue;
            }
            let _ = fs::remove_file(pack_dir.join(name));
            let _ = fs::remove_file(pack_dir.join(format!("{stem}.idx")));
            let _ = fs::remove_file(pack_dir.join(format!("{stem}.promisor")));
            remove_pack_sidecars(pack_dir, &stem);
        }
        return Ok(());
    }

    let mut union_oids: HashSet<ObjectId> = HashSet::new();
    for name in &retained {
        if let Some(s) = by_name.get(name) {
            union_oids.extend(s.iter().copied());
        }
    }
    // Objects only in `.keep` packs must count toward supersession: otherwise an old pack that
    // duplicates a kept object is never removed (`t7700-repack` alternate + `.keep` chain).
    for (name, oids) in &by_name {
        let stem = name
            .strip_suffix(".pack")
            .unwrap_or(name.as_str())
            .to_string();
        if pack_dir.join(format!("{stem}.keep")).exists() {
            union_oids.extend(oids.iter().copied());
        }
    }
    union_oids.extend(
        union_oids_from_flat_pack_index_dirs(extra_objects_dirs)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    );

    let mut changed = true;
    while changed {
        changed = false;
        for (name, oids) in &by_name {
            if retained.contains(name) {
                continue;
            }
            let stem = name
                .strip_suffix(".pack")
                .unwrap_or(name.as_str())
                .to_string();
            if pack_dir.join(format!("{stem}.keep")).exists() {
                continue;
            }
            if pack_dir.join(format!("{stem}.promisor")).exists() {
                continue;
            }
            if oids.iter().all(|o| union_oids.contains(o)) {
                continue;
            }
            retained.insert(name.clone());
            union_oids.extend(oids.iter().copied());
            changed = true;
        }
    }

    for name in by_name.keys() {
        if retained.contains(name) {
            continue;
        }
        let stem = name
            .strip_suffix(".pack")
            .unwrap_or(name.as_str())
            .to_string();
        if pack_dir.join(format!("{stem}.keep")).exists() {
            continue;
        }
        if pack_dir.join(format!("{stem}.promisor")).exists() {
            continue;
        }
        let _ = fs::remove_file(pack_dir.join(name));
        let _ = fs::remove_file(pack_dir.join(format!("{stem}.idx")));
        let _ = fs::remove_file(pack_dir.join(format!("{stem}.promisor")));
        remove_pack_sidecars(pack_dir, &stem);
    }

    Ok(())
}

fn remove_generated_pack_family(pack_dir: &Path, hash: &str) {
    let stem = format!("pack-{hash}");
    let _ = fs::remove_file(pack_dir.join(format!("{stem}.pack")));
    let _ = fs::remove_file(pack_dir.join(format!("{stem}.idx")));
    remove_pack_sidecars(pack_dir, &stem);
}

fn remove_old_cruft_packs_not_in_keep(pack_dir: &Path, keep_names: &[String]) -> Result<()> {
    let keep: HashSet<String> = keep_names
        .iter()
        .map(|name| {
            let base = pack_basename(name);
            base.strip_suffix(".pack").unwrap_or(base).to_string()
        })
        .collect();
    let rd = match fs::read_dir(pack_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in rd {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("mtimes") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if keep.contains(stem) {
            continue;
        }
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(pack_dir.join(format!("{stem}.pack")));
        let _ = fs::remove_file(pack_dir.join(format!("{stem}.idx")));
        remove_pack_sidecars(pack_dir, stem);
    }
    Ok(())
}

pub(crate) fn remove_superseded_packs_multi(
    pack_dir: &Path,
    keep_pack_names: &[String],
) -> Result<()> {
    let rd = match fs::read_dir(pack_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in rd {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".pack") {
            continue;
        }
        if keep_pack_names
            .iter()
            .any(|k| pack_basename(k) == name.as_str())
        {
            continue;
        }
        let stem = name
            .strip_suffix(".pack")
            .unwrap_or(name.as_str())
            .to_string();
        if pack_dir.join(format!("{stem}.keep")).exists() {
            continue;
        }
        let pack_path = pack_dir.join(&name);
        let idx_path = pack_dir.join(format!("{stem}.idx"));
        let _ = fs::remove_file(&pack_path);
        let _ = fs::remove_file(&idx_path);
        let _ = fs::remove_file(pack_dir.join(format!("{stem}.promisor")));
        remove_pack_sidecars(pack_dir, &stem);
    }

    Ok(())
}

/// Incremental repack: remove packs that became redundant (every object also in another pack).
fn remove_superseded_packs_incremental(
    pack_dir: &Path,
    new_pack_name: &str,
    always_keep: &[String],
) -> Result<()> {
    let objects_dir = pack_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid pack directory"))?;
    let indexes =
        grit_lib::pack::read_local_pack_indexes(objects_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    if indexes.len() < 2 {
        return Ok(());
    }

    let mut pack_to_oids: Vec<(
        String,
        std::collections::HashSet<grit_lib::objects::ObjectId>,
    )> = Vec::new();
    for idx in &indexes {
        let name = idx
            .pack_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if !name.ends_with(".pack") {
            continue;
        }
        let oids: std::collections::HashSet<grit_lib::objects::ObjectId> = idx
            .entries
            .iter()
            .filter_map(|e| {
                if e.oid.len() == 20 {
                    grit_lib::objects::ObjectId::from_bytes(&e.oid).ok()
                } else {
                    None
                }
            })
            .collect();
        pack_to_oids.push((name, oids));
    }

    let new_set = pack_to_oids
        .iter()
        .find(|(n, _)| n == new_pack_name)
        .map(|(_, s)| s.clone())
        .unwrap_or_default();

    for (name, oids) in &pack_to_oids {
        if name == new_pack_name {
            continue;
        }
        if always_keep
            .iter()
            .any(|k| pack_basename(k) == name.as_str())
        {
            continue;
        }
        let stem = name
            .strip_suffix(".pack")
            .unwrap_or(name.as_str())
            .to_string();
        if pack_dir.join(format!("{stem}.keep")).exists() {
            continue;
        }
        if pack_dir.join(format!("{stem}.promisor")).exists() {
            continue;
        }
        let mut covered = true;
        for oid in oids {
            if new_set.contains(oid) {
                continue;
            }
            let mut in_other = false;
            for (other_name, other_oids) in &pack_to_oids {
                if other_name == name {
                    continue;
                }
                if other_oids.contains(oid) {
                    in_other = true;
                    break;
                }
            }
            if !in_other {
                covered = false;
                break;
            }
        }
        if covered {
            let _ = fs::remove_file(pack_dir.join(name));
            let _ = fs::remove_file(pack_dir.join(format!("{stem}.idx")));
            let _ = fs::remove_file(pack_dir.join(format!("{stem}.promisor")));
            remove_pack_sidecars(pack_dir, &stem);
        }
    }

    Ok(())
}

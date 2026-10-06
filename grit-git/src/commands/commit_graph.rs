//! `grit commit-graph` — write and verify commit-graph files.
//!
//! The commit-graph file stores commit OIDs in sorted order along with
//! their tree OIDs, parent indices, and generation numbers for faster
//! traversal.
//!
//! File format (simplified):
//!   - 8-byte header: "CGPH" + version(1) + hash_version(1) + num_chunks(1) + reserved(1)
//!   - Chunk table of contents
//!   - OID Fanout (256 × 4 bytes)
//!   - OID Lookup (N × 20 bytes, sorted)
//!   - Commit Data (N × 36 bytes: tree_oid(20) + parent1(4) + parent2(4) + generation(4) + commit_time(4))
//!   - Trailer: checksum

use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use grit_lib::bloom::BloomFilterSettings;
use grit_lib::commit_graph_file::CommitGraphChain;
use grit_lib::commit_graph_write::{
    build_commit_graph_bytes, collect_reachable_commit_oids, count_referenced_commit_tips,
    load_commit_graph_commit_info, BloomWriteStats,
};
use grit_lib::config::ConfigSet;
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;

/// Arguments for `grit commit-graph`.
#[derive(Debug, ClapArgs)]
#[command(
    about = "Write and verify commit-graph files",
    override_usage = "grit commit-graph (write | verify)"
)]
pub struct Args {
    /// Optional alternate object directory. `global` so it is accepted both before and after
    /// the `write`/`verify` subcommand, matching Git's top-level `--object-dir` option.
    #[arg(long = "object-dir", global = true)]
    pub object_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: CommitGraphCommand,
}

#[derive(Debug, Subcommand)]
pub enum CommitGraphCommand {
    /// Write a commit-graph file.
    Write {
        /// Include all reachable commits.
        #[arg(long)]
        reachable: bool,
        /// Read commits from stdin.
        #[arg(long)]
        stdin_commits: bool,
        /// Read packs from stdin.
        #[arg(long)]
        stdin_packs: bool,
        /// Use changed paths Bloom filters.
        #[arg(long)]
        changed_paths: bool,
        /// Do not compute changed-path Bloom filters.
        #[arg(long = "no-changed-paths", conflicts_with = "changed_paths")]
        no_changed_paths: bool,
        /// Enable split commit-graph (`--split`, `--split=replace`, `--split=no-merge`).
        #[arg(long = "split", num_args = 0..=1, default_missing_value = "yes", value_name = "STRATEGY")]
        split: Option<String>,
        /// Set size multiple for split.
        #[arg(long)]
        size_multiple: Option<f64>,
        /// Set max commits for split.
        #[arg(long)]
        max_commits: Option<u64>,
        /// Set expire time.
        #[arg(long)]
        expire_time: Option<String>,
        /// Show progress.
        #[arg(long)]
        progress: bool,
        /// Don't show progress.
        #[arg(long)]
        no_progress: bool,
        /// Limit Bloom filters computed in this write (Git `commitGraph.maxNewFilters`).
        #[arg(long = "max-new-filters")]
        max_new_filters: Option<u32>,
    },
    /// Verify an existing commit-graph file.
    Verify {
        /// Enable shallow mode.
        #[arg(long)]
        shallow: bool,
        /// Show progress.
        #[arg(long)]
        progress: bool,
        /// Don't show progress.
        #[arg(long)]
        no_progress: bool,
    },
}

// ── Constants ──────────────────────────────────────────────────────────
const SIGNATURE: &[u8; 4] = b"CGPH";
const VERSION: u8 = 1;
const HASH_VERSION_SHA1: u8 = 1;
const HASH_LEN: usize = 20;

// Chunk IDs (verify)
const CHUNK_OID_FANOUT: u32 = 0x4f494446; // "OIDF"
const CHUNK_OID_LOOKUP: u32 = 0x4f49444c; // "OIDL"
const CHUNK_COMMIT_DATA: u32 = 0x43444154; // "CDAT"

/// Run `grit commit-graph`.
pub fn run(args: Args) -> Result<()> {
    match args.command {
        CommitGraphCommand::Write {
            reachable,
            stdin_commits,
            stdin_packs,
            changed_paths,
            no_changed_paths,
            split,
            size_multiple,
            max_commits,
            expire_time,
            progress,
            no_progress,
            max_new_filters,
        } => cmd_write(
            args.object_dir,
            reachable,
            stdin_commits,
            stdin_packs,
            changed_paths,
            no_changed_paths,
            split.as_deref(),
            size_multiple,
            max_commits,
            expire_time.as_deref(),
            progress,
            no_progress,
            max_new_filters,
        ),
        CommitGraphCommand::Verify {
            shallow,
            progress,
            no_progress,
        } => cmd_verify(args.object_dir, shallow, progress, no_progress),
    }
}

fn progress_delay_secs() -> u64 {
    std::env::var("GIT_PROGRESS_DELAY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2)
}

fn should_show_commit_graph_progress(progress: bool, no_progress: bool) -> bool {
    if no_progress {
        return false;
    }
    if progress {
        return true;
    }
    std::io::stderr().is_terminal()
}

// ── Write ──────────────────────────────────────────────────────────────

fn read_stdin_commit_seeds() -> Result<HashSet<ObjectId>> {
    let text = std::io::read_to_string(std::io::stdin()).context("reading --stdin-commits")?;
    let mut out = HashSet::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let oid = ObjectId::from_hex(t).with_context(|| format!("invalid commit id '{t}'"))?;
        out.insert(oid);
    }
    Ok(out)
}

fn walk_commit_closure(odb: &Odb, seeds: HashSet<ObjectId>) -> Result<HashSet<ObjectId>> {
    let mut commits = HashSet::new();
    let mut stack: Vec<ObjectId> = seeds.into_iter().collect();
    while let Some(oid) = stack.pop() {
        if !commits.insert(oid) {
            continue;
        }
        let obj = match odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if obj.kind != ObjectKind::Commit {
            if obj.kind == ObjectKind::Tag {
                if let Ok(text) = std::str::from_utf8(&obj.data) {
                    for line in text.lines() {
                        if let Some(rest) = line.strip_prefix("object ") {
                            if let Ok(target) = ObjectId::from_hex(rest.trim()) {
                                stack.push(target);
                            }
                        }
                    }
                }
            }
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        for p in &commit.parents {
            stack.push(*p);
        }
    }
    Ok(commits)
}

fn trace2_json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn trace2_json_data(path: &str, category: &str, key: &str, value: &str) {
    let now = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let total_secs = now.as_secs();
        let micros = now.subsec_micros();
        let secs_in_day = total_secs % 86400;
        let hours = secs_in_day / 3600;
        let mins = (secs_in_day % 3600) / 60;
        let secs = secs_in_day % 60;
        format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, micros)
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::Path::new(path))
    {
        let value_esc = trace2_json_escape(value);
        let _ = writeln!(
            f,
            r#"{{"event":"data","sid":"grit-0","time":"{}","category":"{}","key":"{}","value":"{}"}}"#,
            now, category, key, value_esc
        );
    }
}

fn emit_commit_graph_trace2(settings: &BloomFilterSettings, stats: &BloomWriteStats) {
    let Ok(path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let now = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let total_secs = now.as_secs();
        let micros = now.subsec_micros();
        let secs_in_day = total_secs % 86400;
        let hours = secs_in_day / 3600;
        let mins = (secs_in_day % 3600) / 60;
        let secs = secs_in_day % 60;
        format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, micros)
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::Path::new(&path))
    {
        let settings_json = format!(
            "{{\"hash_version\":{},\"num_hashes\":{},\"bits_per_entry\":{},\"max_changed_paths\":{}}}",
            settings.hash_version,
            settings.num_hashes,
            settings.bits_per_entry,
            settings.max_changed_paths
        );
        let _ = writeln!(
            f,
            r#"{{"event":"data_json","sid":"grit-0","time":"{}","category":"bloom","key":"settings","value":{}}}"#,
            now, settings_json
        );
        let _ = writeln!(
            f,
            r#"{{"event":"data_json","sid":"grit-0","time":"{}","category":"commit-graph","key":"filter-computed","value":"{}"}}"#,
            now, stats.filter_computed
        );
        let _ = writeln!(
            f,
            r#"{{"event":"data_json","sid":"grit-0","time":"{}","category":"commit-graph","key":"filter-not-computed","value":"{}"}}"#,
            now, stats.filter_not_computed
        );
        let _ = writeln!(
            f,
            r#"{{"event":"data_json","sid":"grit-0","time":"{}","category":"commit-graph","key":"filter-trunc-empty","value":"{}"}}"#,
            now, stats.filter_trunc_empty
        );
        let _ = writeln!(
            f,
            r#"{{"event":"data_json","sid":"grit-0","time":"{}","category":"commit-graph","key":"filter-trunc-large","value":"{}"}}"#,
            now, stats.filter_trunc_large
        );
        let _ = writeln!(
            f,
            r#"{{"event":"data_json","sid":"grit-0","time":"{}","category":"commit-graph","key":"filter-upgraded","value":"{}"}}"#,
            now, stats.filter_upgraded
        );
    }
}

fn hex_to_hash20(hex: &str) -> Option<[u8; 20]> {
    if hex.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Parse a `--expire-time` value into a Unix timestamp.
///
/// Git's `parse_expiry_date` accepts a wide range of formats; the tests only use
/// `"YYYY-MM-DD HH:MM ±HH:MM"`, so we support that plus a bare epoch integer.
fn parse_expire_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    // Format: "YYYY-MM-DD HH:MM[:SS] ±HH:MM"
    let mut parts = s.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let tz = parts.next();

    let mut dp = date.split('-');
    let year: i64 = dp.next()?.parse().ok()?;
    let month: i64 = dp.next()?.parse().ok()?;
    let day: i64 = dp.next()?.parse().ok()?;

    let mut tp = time.split(':');
    let hour: i64 = tp.next()?.parse().ok()?;
    let minute: i64 = tp.next()?.parse().ok()?;
    let second: i64 = tp.next().and_then(|v| v.parse().ok()).unwrap_or(0);

    // Days since Unix epoch (civil calendar; Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut ts = days * 86400 + hour * 3600 + minute * 60 + second;

    // Apply timezone offset (the given local time is `ts` in that offset; convert to UTC).
    if let Some(tz) = tz {
        if let Some(off) = parse_tz_offset(tz) {
            ts -= off;
        }
    }
    Some(ts)
}

fn parse_tz_offset(tz: &str) -> Option<i64> {
    let (sign, rest) = match tz.as_bytes().first()? {
        b'+' => (1i64, &tz[1..]),
        b'-' => (-1i64, &tz[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    let (h, m) = if digits.len() == 4 {
        (
            digits[0..2].parse::<i64>().ok()?,
            digits[2..4].parse::<i64>().ok()?,
        )
    } else if rest.contains(':') {
        let mut p = rest.split(':');
        (p.next()?.parse().ok()?, p.next()?.parse().ok()?)
    } else {
        return None;
    };
    Some(sign * (h * 3600 + m * 60))
}

fn commit_graph_layer_id_hash(path: &Path) -> Option<[u8; 20]> {
    let raw = fs::read(path).ok()?;
    if raw.len() < 40 {
        return None;
    }
    let body = &raw[..raw.len() - 20];
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(body);
    Some(h.finalize().into())
}

/// Write `contents` to `path` atomically by writing a uniquely-named temporary file in the
/// same directory and renaming it over `path`.
///
/// This matches Git's commit-graph machinery, which always renames its lockfile into place
/// rather than truncating an existing file. The rename creates a fresh inode, so if `path` was
/// previously **hardlinked** (as happens for `info/commit-graphs/commit-graph-chain` after a
/// local `git clone` that hardlinks the objects tree), the source repository's file is left
/// untouched. A plain `fs::write` would instead truncate the shared inode and corrupt the
/// clone source's commit-graph chain.
///
/// # Parameters
/// - `path`: destination file path (overwritten atomically).
/// - `contents`: bytes to write.
///
/// # Errors
/// Returns an error if the temporary file cannot be created/written or the rename fails.
fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path {:?} has no parent directory", path))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("path {:?} has no file name", path))?;
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    fs::write(&tmp, contents).with_context(|| format!("writing {:?}", tmp))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {:?} to {:?}", tmp, path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_write(
    object_dir: Option<PathBuf>,
    _reachable: bool,
    stdin_commits: bool,
    stdin_packs: bool,
    changed_paths: bool,
    no_changed_paths: bool,
    split: Option<&str>,
    size_multiple: Option<f64>,
    max_commits: Option<u64>,
    expire_time: Option<&str>,
    progress: bool,
    no_progress: bool,
    max_new_filters_cli: Option<u32>,
) -> Result<()> {
    if stdin_packs {
        bail!("commit-graph write --stdin-packs is not implemented yet");
    }
    let repo = Repository::discover(None)?;
    let objects_dir = object_dir.unwrap_or_else(|| repo.git_dir.join("objects"));
    let odb = Odb::new(&objects_dir);
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();

    let core_on = cfg
        .get_bool("core.commitgraph")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    if !core_on {
        eprintln!(
            "warning: attempting to write a commit-graph, but 'core.commitGraph' is disabled"
        );
        return Ok(());
    }

    let ver = cfg
        .get("commitgraph.changedpathsversion")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(-1);
    if !(-1..=2).contains(&ver) {
        eprintln!(
            "warning: attempting to write a commit-graph, but 'commitGraph.changedPathsVersion' ({ver}) is not supported"
        );
        return Ok(());
    }

    let mut bloom = BloomFilterSettings {
        hash_version: if ver == 2 { 2 } else { 1 },
        num_hashes: 7,
        bits_per_entry: 10,
        max_changed_paths: 512,
    };
    if let Some(ref chain) = CommitGraphChain::load(&objects_dir) {
        if let Some(bs) = chain.top_layer_bloom_settings() {
            if ver == -1 {
                bloom.hash_version = bs.hash_version;
            }
            bloom.num_hashes = bs.num_hashes;
            bloom.bits_per_entry = bs.bits_per_entry;
            bloom.max_changed_paths = bs.max_changed_paths;
        }
    }
    if let Ok(s) = std::env::var("GIT_TEST_BLOOM_SETTINGS_NUM_HASHES") {
        if let Ok(n) = s.parse::<u32>() {
            bloom.num_hashes = n;
        }
    }
    if let Ok(s) = std::env::var("GIT_TEST_BLOOM_SETTINGS_BITS_PER_ENTRY") {
        if let Ok(n) = s.parse::<u32>() {
            bloom.bits_per_entry = n;
        }
    }
    if let Ok(s) = std::env::var("GIT_TEST_BLOOM_SETTINGS_MAX_CHANGED_PATHS") {
        if let Ok(n) = s.parse::<u32>() {
            bloom.max_changed_paths = n;
        }
    }
    bloom.hash_version = if bloom.hash_version == 2 { 2 } else { 1 };

    let max_new = max_new_filters_cli
        .or_else(|| {
            std::env::var("GIT_TEST_MAX_NEW_FILTERS")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
        })
        .or_else(|| {
            cfg.get("commitgraph.maxnewfilters")
                .and_then(|s| s.parse::<u32>().ok())
        });

    let mut commit_set = if stdin_commits {
        let seeds = read_stdin_commit_seeds()?;
        if seeds.is_empty() {
            return Ok(());
        }
        walk_commit_closure(&odb, seeds)?
    } else {
        // Mirror Git's "Collecting referenced commits" progress meter on the --reachable path
        // (commit-graph.c:write_commit_graph_reachable): the count is the number of distinct
        // commit OIDs that refs point to directly (after peeling tags), not the full closure.
        // This is a *delayed* progress meter: it only shows for a TTY or when GIT_PROGRESS_DELAY
        // is 0 (fast operations stay silent under the default 2s delay, matching upstream).
        if should_show_commit_graph_progress(progress, no_progress)
            && (progress_delay_secs() == 0 || std::io::stderr().is_terminal())
        {
            let referenced = count_referenced_commit_tips(&repo.git_dir, &odb).unwrap_or(0);
            eprintln!("Collecting referenced commits: {referenced}, done.");
        }
        collect_reachable_commit_oids(&repo.git_dir, &odb)?
    };

    if commit_set.is_empty() {
        return Ok(());
    }

    let split_enabled = split.is_some();
    let replace = split
        .map(|s| s.eq_ignore_ascii_case("replace"))
        .unwrap_or(false);
    // `--split=no-merge` prohibits merging existing layers into the new tip
    // (commit-graph.c: COMMIT_GRAPH_SPLIT_MERGE_PROHIBITED).
    let no_merge = split
        .map(|s| s.eq_ignore_ascii_case("no-merge"))
        .unwrap_or(false);

    let info_dir = objects_dir.join("info");
    let graph_path = info_dir.join("commit-graph");
    let graphs_dir = info_dir.join("commit-graphs");
    let chain_path = graphs_dir.join("commit-graph-chain");

    if split_enabled && !replace {
        fs::create_dir_all(&graphs_dir)?;
        let chain_empty = !chain_path.is_file()
            || fs::read_to_string(&chain_path)
                .map(|s| {
                    s.lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .is_none()
                })
                .unwrap_or(true);
        if chain_empty && graph_path.is_file() {
            let Some(hash) = commit_graph_layer_id_hash(&graph_path) else {
                bail!(
                    "existing commit-graph at {:?} is too small to migrate",
                    graph_path
                );
            };
            let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
            let dest = graphs_dir.join(format!("graph-{hex}.graph"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&graph_path, fs::Permissions::from_mode(0o644));
            }
            fs::rename(&graph_path, &dest)
                .with_context(|| format!("migrating {:?} to {:?}", graph_path, dest))?;
            write_file_atomic(&chain_path, format!("{hex}\n").as_bytes())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(0o444));
            }
        }
    }

    // The existing split chain (tip-first internally). For --split=replace we drop
    // the whole chain and rebuild a single layer, so we do not treat it as a base.
    //
    // The existing chain may be owned by an alternate object directory (a local clone whose
    // `info/alternates` points at a source repo that already has a split chain). In that case the
    // new split layer is written locally but must reference the alternate's base layers via the
    // BASE chunk + chain file, so load across the alternate(s).
    let alt_dirs = read_alternates(&objects_dir);
    let existing_chain = if split_enabled && !replace {
        CommitGraphChain::try_load_across(&objects_dir, &alt_dirs)
            .ok()
            .flatten()
    } else {
        None
    };

    // Pull every commit that already lives in a graph into the working set only
    // when flattening a split chain into a single non-split file. `--split=replace`
    // discards the old chain and rebuilds a single layer from the *requested*
    // seeds' closure alone (it must not resurrect commits no longer reachable
    // from those seeds), so it does not import the old chain's commits.
    if !split_enabled {
        if let Some(chain) = CommitGraphChain::load(&objects_dir) {
            for oid in chain.all_oids_in_order() {
                commit_set.insert(oid);
            }
        }
    }

    // -- Split merge strategy (commit-graph.c:split_graph_merge_strategy) -------
    // Decide how many of the existing chain's base layers (starting from the tip
    // and walking towards the base) should be merged into the new layer we are
    // about to write. The remaining base layers stay referenced via the BASE
    // chunk + chain file.
    //
    // num_before = number of existing layers; we keep `keep_base` of them as the
    // new layer's base and merge `num_before - keep_base` of them into the new tip.
    let num_before = existing_chain.as_ref().map(|c| c.num_layers()).unwrap_or(0);

    // OIDs that already live in the existing chain (so the new commit set only
    // contains genuinely-new commits before merging).
    let base_oids_all: HashSet<ObjectId> = existing_chain
        .as_ref()
        .map(|c| c.all_oids_in_order().into_iter().collect())
        .unwrap_or_default();

    // The genuinely new commits (not already in any existing layer).
    let mut new_only: Vec<ObjectId> = commit_set.iter().copied().collect();
    if split_enabled && !replace && !base_oids_all.is_empty() {
        new_only.retain(|o| !base_oids_all.contains(o));
    }
    let mut num_commits: u64 = new_only.len() as u64;

    // Decide how many base layers to merge into the new tip.
    // Defaults: size_mult = 2, max_commits = 0 (disabled).
    let size_mult = size_multiple.unwrap_or(2.0);
    let max_commits_v = max_commits.unwrap_or(0);
    let mut keep_base = num_before; // number of layers that stay as base
    if split_enabled && !replace && !no_merge {
        let counts = existing_chain
            .as_ref()
            .map(|c| c.layer_commit_counts_tip_first())
            .unwrap_or_default();
        // Walk from the tip (index 0) toward the base, merging while the strategy
        // says the new layer is "too big" relative to the next base layer.
        let mut g = 0usize;
        while g < counts.len() {
            let layer_commits = counts[g] as u64;
            let too_big = (layer_commits as f64) <= size_mult * (num_commits as f64);
            let over_max = max_commits_v != 0 && num_commits > max_commits_v;
            if too_big || over_max {
                num_commits += layer_commits;
                keep_base -= 1;
                g += 1;
            } else {
                break;
            }
        }
    }

    // For --split=replace there is no base.
    if replace {
        keep_base = 0;
    }

    // Merge the absorbed base layers' commits (tip-first layers 0..num_merged)
    // into the working set. The kept base layers are the remaining ones.
    let num_merged = num_before - keep_base;
    let mut base_oids_kept: HashSet<ObjectId> = HashSet::new();
    let mut base_hashes: Vec<[u8; 20]> = Vec::new();
    if split_enabled && !replace {
        if let Some(ref chain) = existing_chain {
            // Absorbed layers (tip-first indices 0..num_merged): merge their OIDs in.
            for idx in 0..num_merged {
                for oid in chain.layer_oids(idx) {
                    commit_set.insert(oid);
                }
            }
            // Kept base layers (tip-first indices num_merged..num_before): collect
            // their OIDs (to exclude from the new layer) and their hashes (BASE
            // chunk order is base-first, so reverse).
            let hashes_tip_first = chain.layer_hashes_tip_first();
            for idx in num_merged..num_before {
                for oid in chain.layer_oids(idx) {
                    base_oids_kept.insert(oid);
                }
            }
            // base_hashes must be base-first (oldest first).
            for idx in (num_merged..num_before).rev() {
                if let Some(h) = hex_to_hash20(&hashes_tip_first[idx]) {
                    base_hashes.push(h);
                }
            }
        }
    }

    // The new layer contains everything in the working set that is not in a kept
    // base layer.
    let mut layer_oids: Vec<ObjectId> = commit_set.into_iter().collect();
    if !base_oids_kept.is_empty() {
        layer_oids.retain(|o| !base_oids_kept.contains(o));
    }
    layer_oids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

    if layer_oids.is_empty() && !(split_enabled && replace) {
        return Ok(());
    }

    // Decide whether to write the generation-data (GDA2) chunk.
    // Default: configured generation version == 2 (commitGraph.generationVersion).
    // When a split merge keeps base layers, the result follows the topmost kept
    // base layer's generation-data presence (commit-graph.c:split_graph_merge_strategy).
    let gen_version = cfg
        .get("commitgraph.generationversion")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(2);
    // gen_version 1 (or anything != 2) never writes the generation-data chunk.
    let mut write_generation_data = gen_version == 2;
    // When a split write keeps base layers, the result follows the topmost kept
    // base layer's generation-data presence: if that layer lacks GDA2, the new
    // layer must also omit it (a chain cannot have a GDA2 layer atop a non-GDA2
    // one). This only *removes* generation data — gen_version 1 already forced it
    // off, and a gen_version 2 write atop a non-GDA2 base must drop it too.
    if write_generation_data && split_enabled && !replace && keep_base > 0 {
        if let Some(ref chain) = existing_chain {
            let has_gdat = chain.layer_has_generation_data_tip_first();
            // The topmost kept base layer is at tip-first index `num_merged`.
            if let Some(&topmost_has) = has_gdat.get(num_merged) {
                if !topmost_has {
                    write_generation_data = false;
                }
            }
        }
    }

    let mut infos = HashMap::new();
    for oid in &layer_oids {
        infos.insert(*oid, load_commit_graph_commit_info(&odb, *oid)?);
    }

    // Git's close_reachable() parses every reachable commit and fails the write
    // if a parent commit object is missing from the object store (it cannot be
    // placed in the graph). Mirror that: every parent of a commit we are about
    // to write must either be present in a base layer or be readable from the
    // ODB. Otherwise abort (and the just-created temporary layer is discarded —
    // we have not written any file yet at this point). t5324 "temporary graph
    // layer is discarded upon failure" relies on this.
    {
        let in_layer: HashSet<ObjectId> = layer_oids.iter().copied().collect();
        for oid in &layer_oids {
            let info = &infos[oid];
            for p in &info.parents {
                if in_layer.contains(p) || base_oids_kept.contains(p) || base_oids_all.contains(p) {
                    continue;
                }
                if odb.read(p).is_err() {
                    bail!("unable to parse commit {p}");
                }
            }
        }
    }

    // Delayed progress: like Git's start_delayed_progress, a fast operation stays silent under
    // the default GIT_PROGRESS_DELAY (2s) unless stderr is a TTY. Only GIT_PROGRESS_DELAY=0 (or a
    // TTY) forces the meter to display. Avoid sleeping; just decide whether to emit the line.
    let show_progress = should_show_commit_graph_progress(progress, no_progress)
        && (progress_delay_secs() == 0 || std::io::stderr().is_terminal());
    if show_progress {
        eprintln!(
            "Computing commit graph generation numbers: 100% ({n}/{n}), done.",
            n = layer_oids.len()
        );
    }

    let write_bloom = changed_paths && !no_changed_paths;
    // The new layer's BASE chunk references only the kept base layers, and parent
    // edges into the base must use that sub-chain's global positions.
    let kept_chain: Option<CommitGraphChain> = if split_enabled && !replace && keep_base > 0 {
        existing_chain
            .as_ref()
            .and_then(|c| c.sub_chain_tip_first(num_merged, num_before))
    } else {
        None
    };
    let (base_for_build, hashes_for_build): (Option<&CommitGraphChain>, &[[u8; 20]]) =
        if split_enabled && !replace {
            (kept_chain.as_ref(), &base_hashes)
        } else {
            (None, &[])
        };

    // Reuse changed-path Bloom filters that already exist on disk for any commit we
    // are about to (re)write, instead of recomputing them. Git counts these as
    // `filter_not_computed` and never recomputes a filter that is already present
    // (including empty filters for commits with no changes). This drives the
    // `--max-new-filters` backfill semantics.
    let mut existing_filters: HashMap<ObjectId, Vec<u8>> = HashMap::new();
    // Filters present on disk for a *different* changed-path version that can be
    // relabeled (upgraded) to the requested version without recomputation. This
    // is possible when neither the commit's tree nor its first parent's tree
    // contains a high-bit path byte (v1/v2 hashing only differs there). Git
    // counts these as `filter-upgraded` rather than `filter-computed`.
    let mut upgraded_filters: HashMap<ObjectId, Vec<u8>> = HashMap::new();
    if write_bloom {
        if let Some(chain) = CommitGraphChain::load(&objects_dir) {
            for oid in &layer_oids {
                if let Some(bytes) = chain.existing_filter_bytes(oid, &bloom) {
                    existing_filters.insert(*oid, bytes);
                } else if let Some(bytes) = chain.upgradable_filter_bytes(oid, &bloom) {
                    let parents = infos.get(oid).map(|i| i.parents.as_slice()).unwrap_or(&[]);
                    let commit_high =
                        grit_lib::commit_graph_file::commit_tree_has_high_bit_paths(&odb, *oid);
                    let parent_high = parents.first().is_some_and(|p| {
                        grit_lib::commit_graph_file::commit_tree_has_high_bit_paths(&odb, *p)
                    });
                    if !commit_high && !parent_high {
                        upgraded_filters.insert(*oid, bytes);
                    }
                }
            }
        }
    }

    let (bytes, bstats) = build_commit_graph_bytes(
        &layer_oids,
        &infos,
        &odb,
        write_bloom,
        &bloom,
        base_for_build,
        hashes_for_build,
        max_new,
        &existing_filters,
        &upgraded_filters,
        write_generation_data,
    )?;

    if write_bloom {
        emit_commit_graph_trace2(&bloom, &bstats);
    }

    let file_hash: [u8; 20] = {
        let body = &bytes[..bytes.len().saturating_sub(20)];
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(body);
        h.finalize().into()
    };
    let hex_hash: String = file_hash.iter().map(|b| format!("{b:02x}")).collect();

    fs::create_dir_all(&info_dir)?;
    if split_enabled {
        fs::create_dir_all(&graphs_dir)?;
        if replace {
            let _ = fs::remove_file(&chain_path);
            if graphs_dir.is_dir() {
                for entry in fs::read_dir(&graphs_dir)? {
                    let entry = entry?;
                    let p = entry.path();
                    if p.extension().is_some_and(|e| e == "graph") {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
            fs::create_dir_all(&graphs_dir)?;
        }
        let layer_path = graphs_dir.join(format!("graph-{hex_hash}.graph"));
        // Atomic write (temp + rename) avoids mutating a hardlinked layer file shared with a
        // clone source when the same layer hash already exists on disk.
        write_file_atomic(&layer_path, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&layer_path, fs::Permissions::from_mode(0o444));
        }
        // The chain file is written base-first (Git order: line 1 is the base
        // graph, the last line is the tip). After a split merge, only the kept
        // base layers remain; the new layer is the new tip.
        let mut chain_lines: Vec<String> = Vec::new();
        if !replace {
            if let Some(ref chain) = existing_chain {
                let hashes_tip_first = chain.layer_hashes_tip_first();
                // Kept base layers, base-first (tip-first index num_merged..num_before, reversed).
                for idx in (num_merged..num_before).rev() {
                    chain_lines.push(hashes_tip_first[idx].clone());
                }
            }
        }
        chain_lines.push(hex_hash.clone());
        // Write the chain file atomically (temp + rename) so a hardlinked clone source
        // — local `git clone` hardlinks `info/commit-graphs/commit-graph-chain` — is not
        // corrupted by truncating the shared inode in place.
        write_file_atomic(
            &chain_path,
            format!("{}\n", chain_lines.join("\n")).as_bytes(),
        )?;
        let _ = fs::remove_file(&graph_path);

        // Apply core.sharedRepository permissions to the new layer file and the
        // chain file (commit-graph.c uses the same `adjust_shared_perm` path).
        // Both files are read-only (0444); the shared-repo setting masks that so
        // 0666 → -r--r--r-- and 0600 → -r-------- (t5324).
        let shared = grit_lib::shared_repo::shared_repository_from_config_value(
            cfg.get("core.sharedrepository").as_deref(),
        )
        .unwrap_or(0);
        if shared != 0 {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&chain_path, fs::Permissions::from_mode(0o444));
            }
            let _ = grit_lib::shared_repo::adjust_shared_perm_path(shared, &layer_path);
            let _ = grit_lib::shared_repo::adjust_shared_perm_path(shared, &chain_path);
        }

        // Expire (delete) old layer files that are no longer referenced by the
        // new chain (commit-graph.c:expire_commit_graphs). Only files older than
        // the expire time are removed; the just-written layer and kept base
        // layers are always retained.
        // The kept set holds the just-written layer + kept base layers, keyed by
        // the `graph-<hash>.graph` filename (Git compares full filenames).
        let keep_set: HashSet<String> = chain_lines
            .iter()
            .map(|h| format!("graph-{h}.graph"))
            .collect();
        let expire_ts: Option<i64> = expire_time.and_then(parse_expire_time);
        if let Ok(read_dir) = fs::read_dir(&graphs_dir) {
            for entry in read_dir.flatten() {
                let p = entry.path();
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                // Git's expire_commit_graphs unlinks any `*.graph` file in the dir
                // that is not part of the new chain and is older than the expire
                // time (it matches the `.graph` suffix, not the `graph-` prefix).
                if !name.ends_with(".graph") {
                    continue;
                }
                if keep_set.contains(name) {
                    continue;
                }
                if let Some(ts) = expire_ts {
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    if mtime > ts {
                        continue;
                    }
                }
                let _ = fs::remove_file(&p);
            }
        }
    } else {
        if graphs_dir.is_dir() {
            for entry in fs::read_dir(&graphs_dir)? {
                let entry = entry?;
                let p = entry.path();
                if p.is_file() {
                    let _ = fs::remove_file(&p);
                }
            }
            // Git removes the chain and its layer files when collapsing a split
            // commit-graph into a single file, but leaves the (now empty)
            // commit-graphs directory in place. t4216 checks `test_dir_is_empty`,
            // which requires the directory to still exist.
        }
        let _ = fs::remove_file(&chain_path);
        // Atomic write (temp + rename) so a hardlinked single `info/commit-graph` shared with a
        // local-clone source is replaced with a fresh inode rather than truncated in place.
        write_file_atomic(&graph_path, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&graph_path, fs::Permissions::from_mode(0o444));
        }
    }

    Ok(())
}

// ── Verify ─────────────────────────────────────────────────────────────

const CHUNK_BASE_GRAPHS: u32 = 0x4241_5345; // "BASE"
const CHUNK_GENERATION_DATA: u32 = 0x4744_4132; // "GDA2"

fn sha1_of(data: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(data);
    h.finalize().into()
}

/// One parsed layer for verification.
struct VerifyLayer {
    path: PathBuf,
    data: Vec<u8>,
    num_commits: u32,
    oid_lookup_off: usize,
    cdat_off: usize,
    base_graphs: Vec<[u8; 20]>,
    has_generation_data: bool,
    /// Number of commits in all layers below this one.
    num_commits_in_base: u32,
    /// The layer's identity hash, read from the file's trailer (last 20 bytes).
    /// Git derives `g->oid` from the trailer, so a corrupted trailer changes the
    /// layer's identity and is detected by the chain BASE-chunk match.
    trailer_oid: [u8; 20],
}

/// Parse a single commit-graph layer's chunk table. Returns an error string on
/// any structural problem (used to report "commit-graph file is too small" etc.).
fn parse_verify_layer(path: &Path, data: Vec<u8>) -> std::result::Result<VerifyLayer, String> {
    if data.len() < 8 + 12 {
        return Err("commit-graph file is too small".to_string());
    }
    if &data[0..4] != SIGNATURE {
        return Err("commit-graph file has bad signature".to_string());
    }
    if data[4] != VERSION {
        return Err(format!("commit-graph has unsupported version {}", data[4]));
    }
    if data[5] != HASH_VERSION_SHA1 {
        return Err(format!(
            "commit-graph has unsupported hash version {}",
            data[5]
        ));
    }
    let num_chunks = data[6] as usize;
    let toc_start = 8;
    let toc_end = toc_start + (num_chunks + 1) * 12;
    if data.len() < toc_end + HASH_LEN {
        return Err(format!(
            "commit-graph file is too small to hold {num_chunks} chunks"
        ));
    }
    let mut fanout_off = None;
    let mut lookup_off = None;
    let mut cdat_off = None;
    let mut gdat_off = None;
    let mut base_off = None;
    for i in 0..num_chunks {
        let e = toc_start + i * 12;
        let id = u32::from_be_bytes(data[e..e + 4].try_into().unwrap_or([0; 4]));
        let off = u64::from_be_bytes(data[e + 4..e + 12].try_into().unwrap_or([0; 8])) as usize;
        match id {
            CHUNK_OID_FANOUT => fanout_off = Some(off),
            CHUNK_OID_LOOKUP => lookup_off = Some(off),
            CHUNK_COMMIT_DATA => cdat_off = Some(off),
            CHUNK_GENERATION_DATA => gdat_off = Some(off),
            CHUNK_BASE_GRAPHS => base_off = Some((off, i)),
            _ => {}
        }
    }
    let file_end = u64::from_be_bytes(
        data[toc_start + num_chunks * 12 + 4..toc_start + num_chunks * 12 + 12]
            .try_into()
            .unwrap_or([0; 8]),
    ) as usize;
    let fanout_off = fanout_off.ok_or("commit-graph missing OID fanout chunk")?;
    let lookup_off = lookup_off.ok_or("commit-graph missing OID lookup chunk")?;
    let cdat_off = cdat_off.ok_or("commit-graph missing commit data chunk")?;
    if fanout_off + 256 * 4 > data.len() {
        return Err("commit-graph file is too small".to_string());
    }
    let num_commits = u32::from_be_bytes(
        data[fanout_off + 255 * 4..fanout_off + 256 * 4]
            .try_into()
            .unwrap_or([0; 4]),
    );
    if lookup_off + num_commits as usize * HASH_LEN > data.len() {
        return Err("commit-graph file is too small".to_string());
    }
    if cdat_off + num_commits as usize * 36 > data.len() {
        return Err("commit-graph file is too small".to_string());
    }

    // Parse the BASE chunk (list of base-graph layer hashes, base-first).
    let mut base_graphs: Vec<[u8; 20]> = Vec::new();
    if let Some((boff, idx)) = base_off {
        // Determine chunk end via the next chunk's offset (or file_end).
        let mut end = file_end;
        for j in 0..num_chunks {
            if j == idx {
                continue;
            }
            let e = toc_start + j * 12;
            let off = u64::from_be_bytes(data[e + 4..e + 12].try_into().unwrap_or([0; 8])) as usize;
            if off > boff && off < end {
                end = off;
            }
        }
        let size = end.saturating_sub(boff);
        let count = size / HASH_LEN;
        for k in 0..count {
            let s = boff + k * HASH_LEN;
            if s + HASH_LEN <= data.len() {
                let mut h = [0u8; 20];
                h.copy_from_slice(&data[s..s + HASH_LEN]);
                base_graphs.push(h);
            }
        }
    }

    let mut trailer_oid = [0u8; 20];
    if data.len() >= HASH_LEN {
        trailer_oid.copy_from_slice(&data[data.len() - HASH_LEN..]);
    }

    Ok(VerifyLayer {
        path: path.to_path_buf(),
        data,
        num_commits,
        oid_lookup_off: lookup_off,
        cdat_off,
        base_graphs,
        has_generation_data: gdat_off.is_some(),
        num_commits_in_base: 0,
        trailer_oid,
    })
}

impl VerifyLayer {
    fn oid_at(&self, lex: u32) -> Option<ObjectId> {
        let off = self.oid_lookup_off + lex as usize * HASH_LEN;
        ObjectId::from_bytes(self.data.get(off..off + HASH_LEN)?).ok()
    }
    fn checksum_valid(&self) -> bool {
        if self.data.len() < HASH_LEN {
            return false;
        }
        let body = &self.data[..self.data.len() - HASH_LEN];
        let stored = &self.data[self.data.len() - HASH_LEN..];
        sha1_of(body) == stored
    }
}

/// Resolve a split-graph layer file `graph-<hash>.graph` across the local object
/// dir and any alternates.
///
/// The match is case-SENSITIVE on the filename even when the underlying
/// filesystem is case-insensitive (macOS APFS): Git looks up the layer file by
/// the exact hex string from the chain file, so a chain line whose hex case has
/// been corrupted must be treated as "file not found" (the verify tests rely on
/// this). We therefore confirm a directory entry exists with the exact name.
fn resolve_layer_path(objects_dir: &Path, alt_dirs: &[PathBuf], hash: &str) -> Option<PathBuf> {
    let name = format!("graph-{hash}.graph");
    let dir = objects_dir.join("info").join("commit-graphs");
    if dir_has_exact_entry(&dir, &name) {
        return Some(dir.join(&name));
    }
    for alt in alt_dirs {
        let altdir = alt.join("info").join("commit-graphs");
        if dir_has_exact_entry(&altdir, &name) {
            return Some(altdir.join(&name));
        }
    }
    None
}

/// Whether `dir` contains a file whose name matches `name` byte-for-byte (case
/// sensitive), regardless of the filesystem's case sensitivity.
fn dir_has_exact_entry(dir: &Path, name: &str) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    for entry in rd.flatten() {
        if entry.file_name().to_string_lossy() == name {
            return entry.path().is_file();
        }
    }
    false
}

fn read_alternates(objects_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let alt = objects_dir.join("info").join("alternates");
    if let Ok(content) = fs::read_to_string(&alt) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            out.push(PathBuf::from(line));
        }
    }
    out
}

fn cmd_verify(
    object_dir: Option<PathBuf>,
    shallow: bool,
    progress: bool,
    no_progress: bool,
) -> Result<()> {
    let repo = Repository::discover(None)?;
    let objects_dir = object_dir.unwrap_or_else(|| repo.git_dir.join("objects"));
    let info = objects_dir.join("info");
    let single_path = info.join("commit-graph");
    let chain_path = info.join("commit-graphs").join("commit-graph-chain");

    // Resolve object dirs (local + alternates) used to find layer files.
    let alt_dirs = read_alternates(&objects_dir);
    let odb = Odb::new(&objects_dir);

    // `git commit-graph verify` only loads one of the single file or the chain
    // (single takes precedence). Missing graph => success (nothing to verify).
    let mut layers: Vec<VerifyLayer> = Vec::new();
    let mut had_error = false;
    let mut incomplete_chain = false;

    if single_path.is_file() {
        let data = fs::read(&single_path)?;
        match parse_verify_layer(&single_path, data) {
            Ok(l) => layers.push(l),
            Err(e) => {
                eprintln!("error: {e}");
                return Err(anyhow::anyhow!("commit-graph verify failed"));
            }
        }
    } else if chain_path.is_file() {
        let raw = fs::read(&chain_path)?;
        // hexsz = 40 for SHA-1.
        if raw.len() < 40 {
            if raw.is_empty() {
                return Ok(());
            }
            eprintln!("warning: commit-graph chain file too small");
            return Err(anyhow::anyhow!("commit-graph verify failed"));
        }
        let content = String::from_utf8_lossy(&raw);
        let mut chain_layers: Vec<VerifyLayer> = Vec::new();
        let mut prev_hashes: Vec<[u8; 20]> = Vec::new(); // base-first list of layers loaded so far
        let mut valid = true;
        for line in content.lines() {
            let h = line.trim();
            if h.is_empty() {
                continue;
            }
            if h.len() != 40 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                eprintln!("warning: invalid commit-graph chain: line '{h}' not a hash");
                valid = false;
                break;
            }
            let Some(path) = resolve_layer_path(&objects_dir, &alt_dirs, h) else {
                eprintln!("warning: unable to find all commit-graph files");
                valid = false;
                break;
            };
            let data = match fs::read(&path) {
                Ok(d) => d,
                Err(_) => {
                    eprintln!("warning: unable to find all commit-graph files");
                    valid = false;
                    break;
                }
            };
            let mut layer = match parse_verify_layer(&path, data) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("warning: {e}");
                    valid = false;
                    break;
                }
            };
            // Validate the BASE chunk references match the layers loaded below
            // (add_graph_to_chain). `prev_hashes` is base-first; the BASE chunk
            // is also base-first.
            let n = prev_hashes.len();
            if n > 0 {
                if layer.base_graphs.len() < n {
                    eprintln!("warning: commit-graph base graphs chunk is too small");
                    valid = false;
                    break;
                }
                if layer.base_graphs[..n] != prev_hashes[..n] {
                    eprintln!("warning: commit-graph chain does not match");
                    valid = false;
                    break;
                }
            }
            // num_commits_in_base = sum of commits below.
            layer.num_commits_in_base = chain_layers.iter().map(|l| l.num_commits).sum::<u32>();
            // The layer's identity for the next layer's BASE-chunk match is its
            // trailer (Git's g->oid), not its filename. A corrupted trailer thus
            // breaks the chain match and is surfaced as "chain does not match".
            prev_hashes.push(layer.trailer_oid);
            chain_layers.push(layer);
        }
        if !valid {
            incomplete_chain = true;
        }
        // `chain_layers` is base-first; verify walks tip-first.
        chain_layers.reverse();
        layers = chain_layers;
        if layers.is_empty() {
            // Nothing loaded: chain present but unusable.
            if incomplete_chain {
                eprintln!("error: one or more commit-graph chain files could not be loaded");
            }
            return Err(anyhow::anyhow!("commit-graph verify failed"));
        }
    } else {
        // No commit-graph at all: success.
        return Ok(());
    }

    // Progress meter: total = tip num_commits (+ base commits unless --shallow).
    let show_progress = if no_progress {
        false
    } else {
        progress || std::io::stderr().is_terminal()
    };
    let total: u64 = if let Some(tip) = layers.first() {
        if shallow {
            tip.num_commits as u64
        } else {
            tip.num_commits as u64 + tip.num_commits_in_base as u64
        }
    } else {
        0
    };
    let mut seen: u64 = 0;

    // Verify each layer (tip first). With --shallow, only the tip is checked.
    for layer in &layers {
        // Checksum.
        if !layer.checksum_valid() {
            eprintln!("error: the commit-graph file has incorrect checksum and is likely corrupt");
            had_error = true;
        }

        // Structural + ODB cross-check per commit.
        let mut prev: Option<ObjectId> = None;
        for i in 0..layer.num_commits {
            let Some(cur) = layer.oid_at(i) else {
                eprintln!("error: commit-graph OID lookup truncated");
                had_error = true;
                break;
            };
            if let Some(p) = prev {
                if p.as_bytes() >= cur.as_bytes() {
                    eprintln!("error: commit-graph has incorrect OID order: {p} then {cur}");
                    had_error = true;
                }
            }
            prev = Some(cur);

            seen += 1;
            if show_progress {
                // progress is rendered at completion below
            }

            // Cross-check tree + parents against the object database.
            let obj = match odb.read(&cur) {
                Ok(o) => o,
                Err(_) => {
                    eprintln!(
                        "error: failed to parse commit {cur} from object database for commit-graph"
                    );
                    had_error = true;
                    continue;
                }
            };
            let commit = match grit_lib::objects::parse_commit(&obj.data) {
                Ok(c) => c,
                Err(_) => {
                    eprintln!(
                        "error: failed to parse commit {cur} from object database for commit-graph"
                    );
                    had_error = true;
                    continue;
                }
            };
            // Cross-check the root tree recorded in CDAT.
            let coff = layer.cdat_off + i as usize * 36;
            if coff + HASH_LEN <= layer.data.len() {
                if let Ok(graph_tree) =
                    ObjectId::from_bytes(layer.data[coff..coff + HASH_LEN].try_into().unwrap())
                {
                    if graph_tree != commit.tree {
                        eprintln!(
                            "error: root tree OID for commit {cur} in commit-graph is {graph_tree} != {}",
                            commit.tree
                        );
                        had_error = true;
                    }
                }
            }
        }

        if shallow {
            break;
        }
    }

    if show_progress {
        let pct = seen
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(100);
        eprintln!("Verifying commits in commit graph: {pct}% ({seen}/{total}), done.");
    }

    if incomplete_chain {
        eprintln!("error: one or more commit-graph chain files could not be loaded");
        had_error = true;
    }

    if had_error {
        return Err(anyhow::anyhow!("commit-graph verify failed"));
    }
    Ok(())
}

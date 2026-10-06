//! `grit index-pack` — build pack index for an existing pack file.
//!
//! Reads a `.pack` file (from a path or stdin), parses all objects, and writes
//! a `.idx` version-2 index file alongside it.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::config::ConfigSet;
use grit_lib::gitmodules;
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use sha1::{Digest, Sha1};
use sha2::{Digest as Sha2Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use grit_lib::odb::Odb;
use grit_lib::pack::{read_pack_index, verify_pack_and_collect};
use grit_lib::pack_rev::{
    build_pack_rev_bytes_from_index_order_offsets_and_checksum, rev_path_for_index,
    verify_pack_rev_file,
};
use grit_lib::unpack_objects::{apply_delta, strict_verify_packed_references};

/// Merge `git index-pack --strict RULE` / `--fsck-objects RULE` into `--strict=RULE` so the pack
/// path is not consumed as the flag value (matches Git's argv shape in the test suite).
pub fn preprocess_argv(rest: &mut Vec<String>) {
    let mut i = 0usize;
    while i < rest.len() {
        if rest[i] == "--strict" || rest[i] == "--fsck-objects" {
            if let Some(next) = rest.get(i + 1) {
                if next.starts_with('-') {
                    let flag = if rest[i] == "--strict" {
                        "--strict"
                    } else {
                        "--fsck-objects"
                    };
                    rest[i] = format!("{flag}=");
                    i += 1;
                    continue;
                }
                if !next.starts_with('-') && next.to_ascii_lowercase().ends_with(".pack") {
                    // Git: `index-pack --strict foo.pack` — `foo.pack` is the pack file, not fsck rules.
                    let flag = if rest[i] == "--strict" {
                        "--strict"
                    } else {
                        "--fsck-objects"
                    };
                    rest[i] = format!("{flag}=");
                    i += 1;
                    continue;
                }
            } else {
                let flag = if rest[i] == "--strict" {
                    "--strict"
                } else {
                    "--fsck-objects"
                };
                rest[i] = format!("{flag}=");
                i += 1;
                continue;
            }
        }
        if i + 1 < rest.len() {
            let next = &rest[i + 1];
            let merge = next.contains('=') && !next.starts_with('-');
            if merge && (rest[i] == "--strict" || rest[i] == "--fsck-objects") {
                let flag = if rest[i] == "--strict" {
                    "--strict"
                } else {
                    "--fsck-objects"
                };
                rest[i] = format!("{flag}={next}");
                rest.remove(i + 1);
                continue;
            }
        }
        i += 1;
    }
}

/// Git accepts `index-pack --strict path.pack`. Ensure the `*.pack` path is the last argv
/// token so clap binds it to the positional `PACK-FILE`.
pub fn normalize_argv_for_positional_pack(rest: &mut Vec<String>) {
    if rest.iter().any(|a| a == "--stdin") {
        return;
    }
    let Some(pi) = rest.iter().position(|a| {
        let lower = a.to_ascii_lowercase();
        lower.ends_with(".pack")
    }) else {
        return;
    };
    let pack = rest.remove(pi);
    rest.push(pack);
}

/// Parse `git index-pack` arguments without the generic clap flattening wrapper.
pub fn parse_argv(mut argv: Vec<String>) -> Result<Args> {
    preprocess_argv(&mut argv);
    normalize_argv_for_positional_pack(&mut argv);
    let mut args = Args {
        stdin: false,
        fix_thin: false,
        pack_file: None,
        extra_pack_files: Vec::new(),
        verify: false,
        verify_stat: false,
        verify_stat_only: false,
        verbose: false,
        object_format: None,
        index_version: None,
        strict: None,
        fsck_objects: None,
        output: None,
        keep: None,
        threads: None,
        max_input_size: None,
        rev_index: false,
        no_rev_index: false,
        pack_header: None,
    };

    let mut i = 0usize;
    while i < argv.len() {
        let arg = &argv[i];
        match arg.as_str() {
            "--stdin" => args.stdin = true,
            "--fix-thin" => args.fix_thin = true,
            "--verify" => args.verify = true,
            "--verify-stat" => args.verify_stat = true,
            "--verify-stat-only" => args.verify_stat_only = true,
            "-v" | "--verbose" => args.verbose = true,
            "--rev-index" => args.rev_index = true,
            "--no-rev-index" => args.no_rev_index = true,
            "--strict" | "--strict=" => args.strict = Some(String::new()),
            "--fsck-objects" | "--fsck-objects=" => args.fsck_objects = Some(String::new()),
            "-o" | "--output" => {
                i += 1;
                let Some(value) = argv.get(i) else {
                    bail!("{arg} requires a value");
                };
                args.output = Some(PathBuf::from(value));
            }
            "--object-format" => {
                i += 1;
                let Some(value) = argv.get(i) else {
                    bail!("--object-format requires a value");
                };
                args.object_format = Some(value.clone());
            }
            "--index-version" => {
                i += 1;
                let Some(value) = argv.get(i) else {
                    bail!("--index-version requires a value");
                };
                args.index_version = Some(value.clone());
            }
            "--keep" => args.keep = Some(String::new()),
            "--threads" => {
                i += 1;
                let Some(value) = argv.get(i) else {
                    bail!("--threads requires a value");
                };
                args.threads = Some(value.parse().context("invalid --threads value")?);
            }
            "--max-input-size" => {
                i += 1;
                let Some(value) = argv.get(i) else {
                    bail!("--max-input-size requires a value");
                };
                args.max_input_size = Some(value.clone());
            }
            _ if arg.starts_with("--object-format=") => {
                args.object_format = Some(arg["--object-format=".len()..].to_owned());
            }
            _ if arg.starts_with("--index-version=") => {
                args.index_version = Some(arg["--index-version=".len()..].to_owned());
            }
            _ if arg.starts_with("--strict=") => {
                args.strict = Some(arg["--strict=".len()..].to_owned());
            }
            _ if arg.starts_with("--fsck-objects=") => {
                args.fsck_objects = Some(arg["--fsck-objects=".len()..].to_owned());
            }
            _ if arg.starts_with("--output=") => {
                args.output = Some(PathBuf::from(&arg["--output=".len()..]));
            }
            _ if arg.starts_with("--keep=") => {
                args.keep = Some(arg["--keep=".len()..].to_owned());
            }
            _ if arg.starts_with("--threads=") => {
                args.threads = Some(
                    arg["--threads=".len()..]
                        .parse()
                        .context("invalid --threads value")?,
                );
            }
            _ if arg.starts_with("--max-input-size=") => {
                args.max_input_size = Some(arg["--max-input-size=".len()..].to_owned());
            }
            "--pack_header" => {
                i += 1;
                let Some(value) = argv.get(i) else {
                    bail!("--pack_header requires a value");
                };
                args.pack_header = Some(value.clone());
            }
            _ if arg.starts_with("--pack_header=") => {
                args.pack_header = Some(arg["--pack_header=".len()..].to_owned());
            }
            _ if arg.starts_with('-') => bail!("unsupported option: {arg}"),
            _ => {
                if args.pack_file.is_some() {
                    args.extra_pack_files.push(arg.clone());
                } else {
                    args.pack_file = Some(arg.clone());
                }
            }
        }
        i += 1;
    }
    Ok(args)
}

/// Arguments for `grit index-pack`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Read pack from stdin and write to objects/pack/.
    #[arg(long)]
    pub stdin: bool,

    /// Fix thin packs by adding missing base objects.
    #[arg(long = "fix-thin")]
    pub fix_thin: bool,

    /// Pack file to index.
    #[arg(value_name = "PACK-FILE")]
    pub pack_file: Option<String>,

    /// Additional pack files supplied by shell globs in verify mode.
    #[arg(skip)]
    pub extra_pack_files: Vec<String>,

    /// Verify the pack file integrity (check all objects).
    #[arg(long = "verify")]
    pub verify: bool,

    /// Like `--verify` but also print delta chain statistics.
    #[arg(long = "verify-stat")]
    pub verify_stat: bool,

    /// Print delta chain statistics only (no per-object listing).
    #[arg(long = "verify-stat-only")]
    pub verify_stat_only: bool,

    /// Verbose progress on stderr (matches Git `-v`; trace2 `region_enter` for progress when
    /// `GIT_TRACE2_EVENT` is set).
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Hash algorithm (accepted for compat, only sha1).
    #[arg(long = "object-format")]
    pub object_format: Option<String>,

    /// Pack index version (`1`, `2`, or `2,<offset>` to force 64-bit offsets).
    #[arg(long = "index-version", value_name = "VER")]
    pub index_version: Option<String>,

    /// Strict mode; optional `key=value` rules after a space are merged by the CLI preprocessor.
    #[arg(long = "strict", num_args = 0..=1, default_missing_value = "", value_name = "RULES")]
    pub strict: Option<String>,

    /// Optional fsck rules (`key=value`); space-separated value is merged by the CLI preprocessor.
    #[arg(
        long = "fsck-objects",
        num_args = 0..=1,
        default_missing_value = "",
        value_name = "RULES"
    )]
    pub fsck_objects: Option<String>,

    /// Write the index to this path instead of alongside the pack.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Write a `.keep` file next to the pack (`--stdin` only; value is the stem suffix).
    #[arg(long = "keep", value_name = "REASON")]
    pub keep: Option<String>,

    /// Thread count (accepted; grit is single-threaded).
    #[arg(long = "threads", value_name = "N")]
    pub threads: Option<u32>,

    /// Reject packs whose on-disk size exceeds this limit (supports `k`/`m`/`g` suffixes; `0` = unlimited).
    #[arg(long = "max-input-size", value_name = "SIZE")]
    pub max_input_size: Option<String>,

    /// Write a `.rev` reverse index next to the `.idx` (overrides `pack.writeReverseIndex` when set).
    #[arg(long = "rev-index")]
    pub rev_index: bool,

    /// Do not write a `.rev` reverse index (overrides `pack.writeReverseIndex` when set).
    #[arg(long = "no-rev-index", conflicts_with = "rev_index")]
    pub no_rev_index: bool,

    /// Pack header (`<version>,<nr>`) supplied by `fetch-pack` after it has already consumed the
    /// 12-byte stream header; the body on stdin then lacks the leading `PACK`/version/count.
    #[arg(long = "pack_header", value_name = "HEADER", hide = true)]
    pub pack_header: Option<String>,
}

/// A resolved pack object.
struct ResolvedObject {
    oid: ObjectId,
    _kind: ObjectKind,
    offset: u64,
    crc32: u32,
}

fn effective_write_rev_index(args: &Args, cfg: &ConfigSet) -> bool {
    if args.no_rev_index {
        return false;
    }
    if args.rev_index {
        return true;
    }
    cfg.pack_write_reverse_index_default()
}

/// Run `grit index-pack`.
pub fn run(args: Args) -> Result<()> {
    warn_threads_and_pack_config(&args);

    if let Some(fmt) = &args.object_format {
        if fmt != "sha1" && fmt != "sha256" {
            bail!("unsupported object format: {fmt}");
        }
    }

    let verify_requested = args.verify || args.verify_stat || args.verify_stat_only;

    // --verify / --verify-stat* modes: verify an existing pack + index.
    if verify_requested {
        return run_verify(&args);
    }

    let pack_raw = if args.stdin {
        let mut buf = Vec::new();
        // `fetch-pack`/`receive-pack` may already have consumed the 12-byte pack stream header
        // and pass it back via `--pack_header=<version>,<nr>`; reconstruct it before the body.
        if let Some(raw_header) = args.pack_header.as_deref() {
            let (version, count) = parse_pack_header_arg(raw_header)?;
            buf.extend_from_slice(b"PACK");
            buf.extend_from_slice(&version.to_be_bytes());
            buf.extend_from_slice(&count.to_be_bytes());
        }
        io::stdin().lock().read_to_end(&mut buf)?;
        buf
    } else if let Some(ref path) = args.pack_file {
        fs::read(path).with_context(|| format!("cannot read {path}"))?
    } else {
        bail!("usage: grit index-pack [--stdin | <pack-file>]");
    };

    if let Some(raw) = args.max_input_size.as_deref() {
        let limit = parse_max_input_size_bytes(raw)?;
        if limit > 0 && (pack_raw.len() as u64) > limit {
            bail!("pack exceeds maximum allowed size ({limit} bytes)");
        }
    }

    if args.verbose {
        eprintln!("Receiving objects: 100%");
        trace2_region_scope("Receiving objects", || Ok(()))?;
    }

    // Validate pack header and checksum; work on body without trailing 20-byte hash.
    if pack_raw.len() < 12 + 20 {
        bail!("pack too small");
    }
    if &pack_raw[0..4] != b"PACK" {
        bail!("not a pack file: invalid signature");
    }
    let version = u32::from_be_bytes(pack_raw[4..8].try_into()?);
    if version != 2 && version != 3 {
        bail!("unsupported pack version {version}");
    }
    let pack_end = pack_raw.len() - 20;
    {
        let mut h = Sha1::new();
        h.update(&pack_raw[..pack_end]);
        let digest = h.finalize();
        if digest.as_slice() != &pack_raw[pack_end..] {
            bail!("pack trailing checksum mismatch");
        }
    }
    let mut pack_data = pack_raw[..pack_end].to_vec();

    let repo = grit_lib::repo::Repository::discover(None).ok();
    let collision_odb = repo.as_ref().map(|r| &r.odb);

    let strict_on = args.strict.is_some();
    let fsck_on = args.fsck_objects.is_some();
    let fsck_ignore_missing_email = args
        .strict
        .as_deref()
        .is_some_and(|s| s == "missingEmail=ignore")
        || args
            .fsck_objects
            .as_deref()
            .is_some_and(|s| s == "missingEmail=ignore");

    let check_collisions = true;
    let (resolved, by_oid) = if args.verbose {
        eprintln!("Resolving deltas: 100%");
        trace2_region_scope("Resolving deltas", || {
            parse_and_resolve(
                &mut pack_data,
                args.fix_thin,
                collision_odb,
                strict_on || fsck_on,
                check_collisions,
                fsck_ignore_missing_email,
            )
        })?
    } else {
        parse_and_resolve(
            &mut pack_data,
            args.fix_thin,
            collision_odb,
            strict_on || fsck_on,
            check_collisions,
            fsck_ignore_missing_email,
        )?
    };

    if strict_on || fsck_on {
        gitmodules::verify_packed_dot_special(&by_oid).map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    let mut pack_bytes = pack_data;
    let mut h = Sha1::new();
    h.update(&pack_bytes);
    pack_bytes.extend_from_slice(h.finalize().as_slice());

    // --strict: reject packs with duplicate objects.
    if strict_on {
        let mut seen = std::collections::HashSet::new();
        for obj in &resolved {
            if !seen.insert(obj.oid) {
                bail!("duplicate object {} found in pack", obj.oid.to_hex());
            }
        }
    }

    if strict_on {
        let odb = repo.as_ref().map(|r| &r.odb);
        strict_verify_packed_references(odb, &by_oid)?;
    }

    // Determine output paths.
    let (pack_path, idx_path) = if args.stdin {
        if let Some(path) = &args.pack_file {
            // Match `git index-pack --stdin <path>`: read pack from stdin and write the
            // resulting fixed pack to the provided path (with adjacent `.idx` by default).
            let pack_out = PathBuf::from(path);
            if let Some(parent) = pack_out.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            fs::write(&pack_out, &pack_bytes)?;
            let idx_out = if let Some(ref o) = args.output {
                o.clone()
            } else {
                let mut p = pack_out.clone();
                p.set_extension("idx");
                p
            };
            (pack_out, idx_out)
        } else {
            // Compute pack checksum to derive filename (trailing 20 bytes).
            let pack_hash = hex::encode(&pack_bytes[pack_bytes.len() - 20..]);
            // We need to discover a repo to find objects/pack/.
            let repo = grit_lib::repo::Repository::discover(None)
                .context("not a git repository (needed for --stdin)")?;
            let pack_dir = repo.odb.objects_dir().join("pack");
            fs::create_dir_all(&pack_dir)?;
            let pack_out = pack_dir.join(format!("pack-{pack_hash}.pack"));
            let mut idx_out = pack_dir.join(format!("pack-{pack_hash}.idx"));
            if let Some(ref o) = args.output {
                idx_out = o.clone();
            }
            fs::write(&pack_out, &pack_bytes)?;
            if let Some(ref reason) = args.keep {
                let keep_name = format!("pack-{pack_hash}.keep");
                fs::write(pack_dir.join(keep_name), reason.as_bytes())?;
            }
            (pack_out, idx_out)
        }
    } else {
        let pack_path = PathBuf::from(
            args.pack_file
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no pack file specified"))?,
        );
        let idx_path = if let Some(ref o) = args.output {
            o.clone()
        } else {
            let mut p = pack_path.clone();
            p.set_extension("idx");
            p
        };
        (pack_path, idx_path)
    };

    if args.keep.is_some() && !args.stdin {
        let mut keep_path = pack_path.clone();
        keep_path.set_extension("keep");
        fs::write(&keep_path, b"")?;
    }

    // Write the .idx file.
    let idx_bytes = build_idx(&resolved, &pack_bytes, args.index_version.as_deref())?;
    fs::write(&idx_path, &idx_bytes)?;

    let cfg = grit_lib::repo::Repository::discover(None)
        .ok()
        .and_then(|r| ConfigSet::load(Some(&r.git_dir), true).ok())
        .unwrap_or_default();
    let write_rev = effective_write_rev_index(&args, &cfg);
    let rev_path = rev_path_for_index(&idx_path);
    if write_rev {
        let mut sorted_entries: Vec<&ResolvedObject> = resolved.iter().collect();
        sorted_entries.sort_by_key(|e| e.oid);
        let idx_order_offsets: Vec<u64> = sorted_entries.iter().map(|e| e.offset).collect();
        let pack_hash_bytes = infer_pack_trailer_bytes(&pack_bytes)?;
        let rev_bytes = build_pack_rev_bytes_from_index_order_offsets_and_checksum(
            &idx_order_offsets,
            &pack_bytes[pack_bytes.len() - pack_hash_bytes..],
        );
        fs::write(&rev_path, rev_bytes)?;
    } else if rev_path.exists() {
        let _ = fs::remove_file(&rev_path);
    }

    // Print the pack hash (matches git index-pack output).
    let pack_checksum = &pack_bytes[pack_bytes.len() - 20..];
    let pack_hex = hex::encode(pack_checksum);
    println!("{pack_hex}");

    let _ = pack_path; // suppress unused warning
    Ok(())
}

std::thread_local! {
    /// Guards against re-entering thin-pack base lazy-fetch while a lazy fetch is already in
    /// flight (the promisor fetch itself ingests a pack with `--fix-thin`).
    static IN_THIN_BASE_LAZY_FETCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Lazily fetch missing REF_DELTA base objects from the promisor remote so a thin pack can be
/// resolved. Returns true if at least one base was fetched. Best-effort: failures (no promisor
/// remote, lazy-fetch disabled, network error) leave the bases missing so the caller bails as before.
///
/// `objects_dir` is the target repo's `objects/` directory (its parent is the git dir), used so the
/// fetch targets the repository being written even when the process cwd differs (`git -C <dir>`).
fn lazy_fetch_thin_delta_bases(objects_dir: Option<&Path>, bases: &[ObjectId]) -> bool {
    if bases.is_empty() {
        return false;
    }
    if IN_THIN_BASE_LAZY_FETCH.with(std::cell::Cell::get) {
        return false;
    }
    let repo = match objects_dir.and_then(|d| d.parent()) {
        Some(git_dir) => match grit_lib::repo::Repository::open(git_dir, None) {
            Ok(r) => r,
            Err(_) => return false,
        },
        None => match grit_lib::repo::Repository::discover(None) {
            Ok(r) => r,
            Err(_) => return false,
        },
    };
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if !grit_lib::promisor::repo_treats_promisor_packs(&repo.git_dir, &config) {
        return false;
    }
    IN_THIN_BASE_LAZY_FETCH.with(|c| c.set(true));
    let result =
        crate::commands::promisor_hydrate::try_lazy_fetch_promisor_objects_batch(&repo, bases);
    IN_THIN_BASE_LAZY_FETCH.with(|c| c.set(false));
    result.is_ok()
}

/// Ingest a `.pack` from `path` into `repo`'s object store (reads the file once).
///
/// Used when ingesting a pack from the network without holding the full response in RAM.
pub(crate) fn ingest_pack_file(
    repo: &grit_lib::repo::Repository,
    path: &std::path::Path,
    fix_thin: bool,
) -> Result<PathBuf> {
    let pack_bytes =
        fs::read(path).with_context(|| format!("read pack file {}", path.display()))?;
    if pack_bytes.len() < 12 + 20 {
        bail!("pack too small");
    }
    if &pack_bytes[0..4] != b"PACK" {
        bail!("not a pack file: invalid signature");
    }
    let version = u32::from_be_bytes(pack_bytes[4..8].try_into()?);
    if version != 2 && version != 3 {
        bail!("unsupported pack version {version}");
    }
    let pack_end = pack_bytes.len() - 20;
    {
        let mut h = Sha1::new();
        h.update(&pack_bytes[..pack_end]);
        let digest = h.finalize();
        if digest.as_slice() != &pack_bytes[pack_end..] {
            bail!("pack trailing checksum mismatch");
        }
    }
    let pack_hash = hex::encode(&pack_bytes[pack_end..]);
    let pack_dir = repo.odb.objects_dir().join("pack");
    fs::create_dir_all(&pack_dir)?;
    let pack_out = pack_dir.join(format!("pack-{pack_hash}.pack"));
    let idx_out = pack_dir.join(format!("pack-{pack_hash}.idx"));
    if path != pack_out {
        fs::rename(path, &pack_out).or_else(|_| {
            fs::copy(path, &pack_out)?;
            fs::remove_file(path)?;
            Ok::<(), std::io::Error>(())
        })?;
    }
    let mut pack_data = pack_bytes[..pack_end].to_vec();
    let (resolved, _by_oid) = parse_and_resolve(
        &mut pack_data,
        fix_thin,
        Some(&repo.odb),
        false,
        true,
        false,
    )?;
    let mut h = Sha1::new();
    h.update(&pack_data);
    let digest = h.finalize();
    let on_disk = if digest.as_slice() == &pack_bytes[pack_end..] {
        pack_bytes
    } else {
        pack_data.extend_from_slice(digest.as_slice());
        fs::write(&pack_out, &pack_data)?;
        pack_data
    };
    if !idx_out.exists() {
        let idx_bytes = build_idx(&resolved, &on_disk, None)?;
        fs::write(&idx_out, &idx_bytes)?;
    }
    Ok(pack_out)
}

pub(crate) fn ingest_pack_bytes(
    repo: &grit_lib::repo::Repository,
    pack_bytes: &[u8],
    fix_thin: bool,
) -> Result<PathBuf> {
    if pack_bytes.len() < 12 + 20 {
        bail!("pack too small");
    }
    if &pack_bytes[0..4] != b"PACK" {
        bail!("not a pack file: invalid signature");
    }
    let version = u32::from_be_bytes(pack_bytes[4..8].try_into()?);
    if version != 2 && version != 3 {
        bail!("unsupported pack version {version}");
    }
    let pack_end = pack_bytes.len() - 20;
    let mut pack_data = pack_bytes[..pack_end].to_vec();
    let (resolved, _by_oid) = parse_and_resolve(
        &mut pack_data,
        fix_thin,
        Some(&repo.odb),
        false,
        true,
        false,
    )?;
    let mut h = Sha1::new();
    h.update(&pack_data);
    pack_data.extend_from_slice(h.finalize().as_slice());
    let pack_dir = repo.odb.objects_dir().join("pack");
    fs::create_dir_all(&pack_dir)?;
    let pack_hash = hex::encode(&pack_data[pack_data.len() - 20..]);
    let pack_out = pack_dir.join(format!("pack-{pack_hash}.pack"));
    let idx_out = pack_dir.join(format!("pack-{pack_hash}.idx"));
    fs::write(&pack_out, &pack_data)?;
    let idx_bytes = build_idx(&resolved, &pack_data, None)?;
    fs::write(&idx_out, &idx_bytes)?;
    Ok(pack_out)
}

fn warn_threads_and_pack_config(args: &Args) {
    let cfg = grit_lib::repo::Repository::discover(None)
        .ok()
        .and_then(|r| ConfigSet::load(Some(&r.git_dir), true).ok())
        .unwrap_or_default();
    if let Some(n) = args.threads {
        eprintln!("warning: no threads support, ignoring --threads={n}");
    }
    if let Some(v) = cfg.get("pack.threads") {
        if v != "0" && v.parse::<u32>().unwrap_or(1) != 0 {
            eprintln!("warning: no threads support, ignoring pack.threads");
        }
    }
}

fn big_file_threshold_bytes() -> u64 {
    grit_lib::repo::Repository::discover(None)
        .ok()
        .and_then(|r| ConfigSet::load(Some(&r.git_dir), true).ok())
        .and_then(|c| c.get("core.bigfilethreshold"))
        .and_then(|s| parse_byte_suffix(&s))
        .unwrap_or(512 * 1024 * 1024)
}

fn parse_max_input_size_bytes(raw: &str) -> Result<u64> {
    let v = grit_lib::config::parse_i64(raw.trim()).map_err(|e| anyhow::anyhow!(e))?;
    if v < 0 {
        bail!("max-input-size must be non-negative");
    }
    Ok(v as u64)
}

/// Parse a `--pack_header=<version>,<nr>` value into `(version, object_count)`.
fn parse_pack_header_arg(raw: &str) -> Result<(u32, u32)> {
    let (version, count) = raw
        .split_once(',')
        .ok_or_else(|| anyhow::anyhow!("invalid --pack_header value '{raw}'"))?;
    let version = version
        .parse::<u32>()
        .with_context(|| format!("invalid --pack_header version '{version}'"))?;
    let count = count
        .parse::<u32>()
        .with_context(|| format!("invalid --pack_header count '{count}'"))?;
    Ok((version, count))
}

fn parse_byte_suffix(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let s_lower = s.to_ascii_lowercase();
    let (num, mult) = if let Some(stripped) = s_lower.strip_suffix('k') {
        (stripped, 1024u64)
    } else if let Some(stripped) = s_lower.strip_suffix('m') {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = s_lower.strip_suffix('g') {
        (stripped, 1024 * 1024 * 1024)
    } else {
        (s, 1u64)
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .map(|n| n.saturating_mul(mult))
}

fn check_sha1_collision_with_odb(
    odb: &Odb,
    kind: ObjectKind,
    data: &[u8],
    oid: &ObjectId,
) -> Result<()> {
    if !matches!(kind, ObjectKind::Blob) || !odb.exists(oid) {
        return Ok(());
    }
    let threshold = big_file_threshold_bytes();
    if (data.len() as u64) <= threshold {
        let existing = odb.read(oid)?;
        if existing.kind != kind || existing.data.len() != data.len() {
            bail!("SHA1 COLLISION FOUND WITH {} !", oid.to_hex());
        }
        if existing.data.as_slice() != data {
            bail!("SHA1 COLLISION FOUND WITH {} !", oid.to_hex());
        }
        return Ok(());
    }
    let existing = odb.read(oid)?;
    if existing.kind != kind || existing.data.len() != data.len() {
        bail!("SHA1 COLLISION FOUND WITH {} !", oid.to_hex());
    }
    if existing.data.as_slice() != data {
        bail!("SHA1 COLLISION FOUND WITH {} !", oid.to_hex());
    }
    Ok(())
}

fn validate_commit_fsck(data: &[u8], ignore_missing_email: bool) -> Result<()> {
    if ignore_missing_email {
        return Ok(());
    }
    let c = parse_commit(data).map_err(|e| anyhow::anyhow!("{e}"))?;
    let author_ok = c.author.contains('@');
    let committer_ok = c.committer.contains('@');
    if !author_ok || !committer_ok {
        bail!("fsck error in packed object");
    }
    Ok(())
}

fn warn_tag_fsck(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let header = text.split_once("\n\n").map(|(h, _)| h).unwrap_or(&text);
    if !header.lines().any(|line| line.starts_with("tagger ")) {
        eprintln!("warning: object missing expected 'tagger' line");
    }
}

fn encode_pack_object_header(buf: &mut Vec<u8>, type_code: u8, payload_len: usize) {
    let mut size = payload_len;
    let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
    size >>= 4;
    if size > 0 {
        buf.push(first | 0x80);
        while size > 0 {
            let b = (size & 0x7f) as u8;
            size >>= 7;
            buf.push(if size > 0 { b | 0x80 } else { b });
        }
    } else {
        buf.push(first);
    }
}

/// Append a full zlib-compressed object to `pack_body` (no trailing pack checksum).
/// Returns offset, object id, and CRC32 of the raw pack entry bytes.
fn append_full_object_to_pack(
    pack_body: &mut Vec<u8>,
    kind: ObjectKind,
    data: &[u8],
) -> Result<(u64, ObjectId, u32)> {
    let offset = pack_body.len() as u64;
    let type_code: u8 = match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    };
    encode_pack_object_header(pack_body, type_code, data.len());
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    let compressed = enc.finish()?;
    pack_body.extend_from_slice(&compressed);
    let oid = Odb::hash_object_data(kind, data);
    let crc = crc32_slice(&pack_body[offset as usize..]);
    Ok((offset, oid, crc))
}

/// Parse all pack objects and resolve deltas. When `fix_thin`, missing `REF_DELTA` bases are read
/// from the ODB and appended to the pack (Git `index-pack --fix-thin`), and the object count in
/// the header is updated.
#[allow(clippy::type_complexity)]
fn parse_and_resolve(
    pack_body: &mut Vec<u8>,
    fix_thin: bool,
    collision_odb: Option<&Odb>,
    check_collision_and_fsck: bool,
    check_sha1_collisions: bool,
    fsck_ignore_missing_email: bool,
) -> Result<(
    Vec<ResolvedObject>,
    std::collections::HashMap<ObjectId, (ObjectKind, Vec<u8>)>,
)> {
    use std::collections::HashMap;

    if pack_body.len() < 12 {
        bail!("pack too small");
    }
    let nr_objects_initial = u32::from_be_bytes(pack_body[8..12].try_into()?) as usize;

    // For CRC32 we need to track the byte range of each object entry in the pack.
    let mut entries: Vec<(u64, u8, usize, Vec<u8>, Option<ObjectId>, Option<u64>)> = Vec::new();
    // (offset, type_code, header_size, data, base_oid_for_ref, base_offset_for_ofs)

    let mut pos = 12usize; // skip header

    for _ in 0..nr_objects_initial {
        let obj_start = pos;
        let (type_code, _size, data, base_oid, base_offset) =
            read_pack_entry(pack_body, &mut pos, obj_start as u64)?;
        let obj_end = pos;

        // CRC32 over the raw bytes of this entry.
        let _crc = crc32_slice(&pack_body[obj_start..obj_end]);

        entries.push((
            obj_start as u64,
            type_code,
            obj_end - obj_start,
            data,
            base_oid,
            base_offset,
        ));
    }

    if pos != pack_body.len() {
        bail!("junk after pack objects");
    }

    // Resolve: first non-delta objects, then iteratively resolve deltas.
    let mut by_offset: HashMap<u64, (ObjectKind, Vec<u8>)> = HashMap::new();
    let mut by_oid: HashMap<ObjectId, (ObjectKind, Vec<u8>)> = HashMap::new();
    let mut resolved: Vec<ResolvedObject> = Vec::new();
    let mut pending: Vec<(
        u64,
        u8,
        Vec<u8>,
        Option<ObjectId>,
        Option<u64>,
        usize,
        usize,
    )> = Vec::new();

    // Try to open repo ODB for fix-thin. Prefer the caller-supplied ODB (the actual target repo,
    // e.g. when ingesting a fetched pack for `git -C <dir> fetch`) over re-discovering from the
    // process cwd, which may differ from the repository being written.
    let fix_thin_objects_dir: Option<PathBuf> = if fix_thin {
        match collision_odb {
            Some(o) => Some(o.objects_dir().to_path_buf()),
            None => grit_lib::repo::Repository::discover(None)
                .ok()
                .map(|r| r.odb.objects_dir().to_path_buf()),
        }
    } else {
        None
    };
    let mut odb = fix_thin_objects_dir.as_deref().map(Odb::new);

    for (offset, type_code, _entry_len, data, base_oid, base_offset) in &entries {
        let obj_start = *offset as usize;
        let obj_end = obj_start + _entry_len;
        let crc = crc32_slice(&pack_body[obj_start..obj_end]);

        match type_code {
            1..=4 => {
                let kind = type_code_to_kind(*type_code)?;
                let oid = Odb::hash_object_data(kind, data);
                if check_collision_and_fsck || check_sha1_collisions {
                    if let Some(odb) = collision_odb {
                        check_sha1_collision_with_odb(odb, kind, data, &oid)?;
                    }
                }
                if check_collision_and_fsck {
                    if kind == ObjectKind::Commit {
                        validate_commit_fsck(data, fsck_ignore_missing_email)?;
                    } else if kind == ObjectKind::Tag {
                        warn_tag_fsck(data);
                    }
                }
                by_offset.insert(*offset, (kind, data.clone()));
                by_oid.insert(oid, (kind, data.clone()));
                resolved.push(ResolvedObject {
                    oid,
                    _kind: kind,
                    offset: *offset,
                    crc32: crc,
                });
            }
            6 | 7 => {
                pending.push((
                    *offset,
                    *type_code,
                    data.clone(),
                    *base_oid,
                    *base_offset,
                    crc as usize, // smuggle crc
                    0,
                ));
            }
            other => bail!("unknown pack type {other}"),
        }
    }

    // Iterative delta resolution, with optional thin-pack base injection.
    let mut remaining = pending;
    loop {
        if remaining.is_empty() {
            break;
        }

        if fix_thin {
            // A REF_DELTA base that is missing locally but lives on a promisor remote (e.g. a blob
            // filtered out of a partial clone) must be lazily fetched so the thin-pack delta can be
            // resolved. Fetch only the genuinely-missing bases; bases the client already has are
            // read from the ODB, never re-fetched (t5616 REF_DELTA test asserts `want
            // <deltabase_missing>` is sent but `want <deltabase_have>` is not).
            if let Some(ref o) = odb {
                let mut missing_bases: Vec<ObjectId> = Vec::new();
                for (_, type_code, _, base_oid_opt, _, _, _) in &remaining {
                    if *type_code != 7 {
                        continue;
                    }
                    let Some(bo) = base_oid_opt else {
                        continue;
                    };
                    if by_oid.contains_key(bo) {
                        continue;
                    }
                    if o.read(bo).is_err() {
                        missing_bases.push(*bo);
                    }
                }
                missing_bases.sort();
                missing_bases.dedup();
                if !missing_bases.is_empty()
                    && lazy_fetch_thin_delta_bases(fix_thin_objects_dir.as_deref(), &missing_bases)
                {
                    // Re-open the ODB so reads see the just-fetched promisor pack(s).
                    odb = fix_thin_objects_dir.as_deref().map(Odb::new);
                }
            }
            if let Some(ref o) = odb {
                let mut bases_to_add: Vec<ObjectId> = Vec::new();
                for (_, type_code, _, base_oid_opt, _, _, _) in &remaining {
                    if *type_code != 7 {
                        continue;
                    }
                    let Some(bo) = base_oid_opt else {
                        continue;
                    };
                    if by_oid.contains_key(bo) {
                        continue;
                    }
                    if o.read(bo).is_ok() {
                        bases_to_add.push(*bo);
                    }
                }
                bases_to_add.sort();
                bases_to_add.dedup();
                for bo in bases_to_add {
                    let obj = o.read(&bo)?;
                    if check_collision_and_fsck || check_sha1_collisions {
                        if let Some(codb) = collision_odb {
                            check_sha1_collision_with_odb(codb, obj.kind, &obj.data, &bo)?;
                        }
                    }
                    if check_collision_and_fsck {
                        if obj.kind == ObjectKind::Commit {
                            validate_commit_fsck(&obj.data, fsck_ignore_missing_email)?;
                        } else if obj.kind == ObjectKind::Tag {
                            warn_tag_fsck(&obj.data);
                        }
                    }
                    let (new_off, oid, crc) =
                        append_full_object_to_pack(pack_body, obj.kind, &obj.data)?;
                    if oid != bo {
                        bail!(
                            "object hash mismatch when appending thin-pack base (expected {}, got {})",
                            bo.to_hex(),
                            oid.to_hex()
                        );
                    }
                    by_offset.insert(new_off, (obj.kind, obj.data.clone()));
                    by_oid.insert(bo, (obj.kind, obj.data));
                    resolved.push(ResolvedObject {
                        oid: bo,
                        _kind: obj.kind,
                        offset: new_off,
                        crc32: crc,
                    });
                    let nr = u32::from_be_bytes(pack_body[8..12].try_into()?) + 1;
                    pack_body[8..12].copy_from_slice(&nr.to_be_bytes());
                }
            }
        }

        let before = remaining.len();
        let mut still_pending = Vec::new();

        for (offset, type_code, delta_data, base_oid_opt, base_offset_opt, crc_smuggled, _) in
            remaining
        {
            let base = if type_code == 6 {
                // OFS_DELTA
                base_offset_opt.and_then(|bo| by_offset.get(&bo).cloned())
            } else {
                // REF_DELTA
                base_oid_opt.and_then(|bo| {
                    by_oid.get(&bo).cloned().or_else(|| {
                        odb.as_ref()
                            .and_then(|o| o.read(&bo).ok())
                            .map(|obj| (obj.kind, obj.data))
                    })
                })
            };

            if let Some((base_kind, base_data)) = base {
                let result_data = apply_delta(&base_data, &delta_data)
                    .map_err(|e| anyhow::anyhow!("delta apply failed: {e}"))?;
                let oid = Odb::hash_object_data(base_kind, &result_data);
                if check_collision_and_fsck || check_sha1_collisions {
                    if let Some(odb) = collision_odb {
                        check_sha1_collision_with_odb(odb, base_kind, &result_data, &oid)?;
                    }
                }
                if check_collision_and_fsck {
                    if base_kind == ObjectKind::Commit {
                        validate_commit_fsck(&result_data, fsck_ignore_missing_email)?;
                    } else if base_kind == ObjectKind::Tag {
                        warn_tag_fsck(&result_data);
                    }
                }
                by_offset.insert(offset, (base_kind, result_data.clone()));
                by_oid.insert(oid, (base_kind, result_data));
                resolved.push(ResolvedObject {
                    oid,
                    _kind: base_kind,
                    offset,
                    crc32: crc_smuggled as u32,
                });
            } else {
                still_pending.push((
                    offset,
                    type_code,
                    delta_data,
                    base_oid_opt,
                    base_offset_opt,
                    crc_smuggled,
                    0,
                ));
            }
        }

        remaining = still_pending;
        if remaining.len() == before {
            bail!(
                "{} delta(s) could not be resolved (use --fix-thin?)",
                remaining.len()
            );
        }
    }

    Ok((resolved, by_oid))
}

/// Read a single pack entry starting at `pos`, return (type_code, size, decompressed_data, base_oid, base_offset).
#[allow(clippy::type_complexity)]
fn read_pack_entry(
    pack_bytes: &[u8],
    pos: &mut usize,
    this_offset: u64,
) -> Result<(u8, usize, Vec<u8>, Option<ObjectId>, Option<u64>)> {
    use flate2::read::ZlibDecoder;

    let c = pack_bytes
        .get(*pos)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("truncated pack"))?;
    *pos += 1;
    let type_code = (c >> 4) & 0x7;
    let mut size = (c & 0x0f) as usize;
    let mut shift = 4u32;
    let mut cur = c;
    while cur & 0x80 != 0 {
        cur = pack_bytes
            .get(*pos)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("truncated pack header"))?;
        *pos += 1;
        size |= ((cur & 0x7f) as usize) << shift;
        shift += 7;
    }

    let mut base_oid = None;
    let mut base_offset = None;

    match type_code {
        6 => {
            // OFS_DELTA: read negative offset.
            let mut c2 = pack_bytes
                .get(*pos)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("truncated ofs-delta"))?;
            *pos += 1;
            let mut value = (c2 & 0x7f) as u64;
            while c2 & 0x80 != 0 {
                c2 = pack_bytes
                    .get(*pos)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("truncated ofs-delta"))?;
                *pos += 1;
                value = ((value + 1) << 7) | (c2 & 0x7f) as u64;
            }
            base_offset = Some(
                this_offset
                    .checked_sub(value)
                    .ok_or_else(|| anyhow::anyhow!("ofs-delta base underflow"))?,
            );
        }
        7 => {
            // REF_DELTA: 20-byte base OID.
            if *pos + 20 > pack_bytes.len() {
                bail!("truncated ref-delta base");
            }
            base_oid = Some(
                ObjectId::from_bytes(&pack_bytes[*pos..*pos + 20])
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            );
            *pos += 20;
        }
        _ => {}
    }

    // Decompress.
    let slice = &pack_bytes[*pos..];
    let mut decoder = ZlibDecoder::new(slice);
    let mut data = Vec::with_capacity(size);
    decoder
        .read_to_end(&mut data)
        .map_err(|e| anyhow::anyhow!("zlib: {e}"))?;
    *pos += decoder.total_in() as usize;

    Ok((type_code, size, data, base_oid, base_offset))
}

fn type_code_to_kind(code: u8) -> Result<ObjectKind> {
    match code {
        1 => Ok(ObjectKind::Commit),
        2 => Ok(ObjectKind::Tree),
        3 => Ok(ObjectKind::Blob),
        4 => Ok(ObjectKind::Tag),
        _ => bail!("type code {code} is not a base object type"),
    }
}

/// Compute CRC32 (IEEE) of a byte slice.
fn crc32_slice(data: &[u8]) -> u32 {
    // CRC32 IEEE polynomial, same as used in pack idx v2.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = CRC32_TABLE[idx] ^ (crc >> 8);
    }
    !crc
}

/// Pre-computed CRC32 lookup table (IEEE 802.3 polynomial 0xEDB88320).
static CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

enum IndexVersion {
    V1,
    V2 { large_offset_threshold: u64 },
}

fn parse_index_version(raw: Option<&str>) -> Result<IndexVersion> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(IndexVersion::V2 {
            large_offset_threshold: 0x8000_0000,
        });
    };
    if raw == "1" {
        return Ok(IndexVersion::V1);
    }
    if raw == "2" {
        return Ok(IndexVersion::V2 {
            large_offset_threshold: 0x8000_0000,
        });
    }
    if let Some(rest) = raw.strip_prefix("2,") {
        if rest.is_empty() {
            bail!("invalid index version: {raw}");
        }
        let threshold = rest
            .strip_prefix("0x")
            .and_then(|hex| u64::from_str_radix(hex, 16).ok())
            .or_else(|| rest.parse::<u64>().ok())
            .ok_or_else(|| anyhow::anyhow!("invalid index version: {raw}"))?;
        return Ok(IndexVersion::V2 {
            large_offset_threshold: threshold,
        });
    }
    bail!("unsupported index version: {raw}")
}

fn build_idx(
    entries: &[ResolvedObject],
    pack_bytes: &[u8],
    raw_version: Option<&str>,
) -> Result<Vec<u8>> {
    match parse_index_version(raw_version)? {
        IndexVersion::V1 => build_idx_v1(entries, pack_bytes),
        IndexVersion::V2 {
            large_offset_threshold,
        } => build_idx_v2(entries, pack_bytes, large_offset_threshold),
    }
}

fn build_idx_v1(entries: &[ResolvedObject], pack_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut sorted: Vec<&ResolvedObject> = entries.iter().collect();
    sorted.sort_by_key(|e| e.oid);

    let mut buf = Vec::new();
    let mut fanout = [0u32; 256];
    for entry in &sorted {
        fanout[entry.oid.as_bytes()[0] as usize] += 1;
    }
    for i in 1..256 {
        fanout[i] += fanout[i - 1];
    }
    for slot in &fanout {
        buf.extend_from_slice(&slot.to_be_bytes());
    }
    for entry in &sorted {
        if entry.offset > u64::from(u32::MAX) {
            bail!("pack too large for index version 1");
        }
        buf.extend_from_slice(&(entry.offset as u32).to_be_bytes());
        buf.extend_from_slice(entry.oid.as_bytes());
    }
    let hash_bytes = infer_pack_trailer_bytes(pack_bytes)?;
    buf.extend_from_slice(&pack_bytes[pack_bytes.len() - hash_bytes..]);
    append_idx_checksum(&mut buf, hash_bytes);
    Ok(buf)
}

/// Build a version-2 `.idx` file from resolved entries and pack bytes.
fn build_idx_v2(
    entries: &[ResolvedObject],
    pack_bytes: &[u8],
    large_offset_threshold: u64,
) -> Result<Vec<u8>> {
    // Sort by OID.
    let mut sorted: Vec<&ResolvedObject> = entries.iter().collect();
    sorted.sort_by_key(|e| e.oid);

    let mut buf: Vec<u8> = Vec::new();

    // Header: magic + version.
    buf.extend_from_slice(&[0xFF, b't', b'O', b'c']);
    buf.extend_from_slice(&2u32.to_be_bytes());

    // Fanout table (256 entries).
    let mut fanout = [0u32; 256];
    for entry in &sorted {
        let first_byte = entry.oid.as_bytes()[0] as usize;
        fanout[first_byte] += 1;
    }
    // Cumulative.
    for i in 1..256 {
        fanout[i] += fanout[i - 1];
    }
    for slot in &fanout {
        buf.extend_from_slice(&slot.to_be_bytes());
    }

    // OID table.
    for entry in &sorted {
        buf.extend_from_slice(entry.oid.as_bytes());
    }

    // CRC32 table.
    for entry in &sorted {
        buf.extend_from_slice(&entry.crc32.to_be_bytes());
    }

    // Offset table (32-bit). Large offsets get MSB set.
    let mut large_offsets: Vec<u64> = Vec::new();
    for entry in &sorted {
        if entry.offset >= large_offset_threshold {
            let idx = large_offsets.len() as u32;
            buf.extend_from_slice(&(idx | 0x8000_0000).to_be_bytes());
            large_offsets.push(entry.offset);
        } else {
            buf.extend_from_slice(&(entry.offset as u32).to_be_bytes());
        }
    }

    // Large offset table.
    for off in &large_offsets {
        buf.extend_from_slice(&off.to_be_bytes());
    }

    // Pack checksum (the pack file's trailing hash) and index checksum both use
    // the repository hash width, inferred from the pack trailer (20 for SHA-1,
    // 32 for SHA-256).
    let hash_bytes = infer_pack_trailer_bytes(pack_bytes)?;
    let pack_checksum = &pack_bytes[pack_bytes.len() - hash_bytes..];
    buf.extend_from_slice(pack_checksum);

    append_idx_checksum(&mut buf, hash_bytes);

    Ok(buf)
}

/// Append the trailing index checksum, hashing the index body so far with the
/// algorithm implied by `hash_bytes` (SHA-1 for 20, SHA-256 for 32).
fn append_idx_checksum(buf: &mut Vec<u8>, hash_bytes: usize) {
    match hash_bytes {
        32 => {
            let mut h = Sha256::new();
            h.update(&*buf);
            let digest = h.finalize();
            buf.extend_from_slice(digest.as_slice());
        }
        _ => {
            let mut h = Sha1::new();
            h.update(&*buf);
            let digest = h.finalize();
            buf.extend_from_slice(digest.as_slice());
        }
    }
}

fn reject_sha256_idx_without_flag(idx_path: &std::path::Path, args: &Args) -> Result<()> {
    if args.object_format.as_deref() == Some("sha256") {
        return Ok(());
    }
    if let Ok(repo) = grit_lib::repo::Repository::discover(None) {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        if cfg
            .get("extensions.objectformat")
            .or_else(|| cfg.get("extensions.objectFormat"))
            .is_some_and(|v| v.eq_ignore_ascii_case("sha256"))
        {
            return Ok(());
        }
    }
    let idx = read_pack_index(idx_path)?;
    if idx.hash_bytes == 32 {
        bail!("wrong index v2 file size in {}", idx_path.display());
    }
    Ok(())
}

fn infer_pack_trailer_bytes(pack_bytes: &[u8]) -> Result<usize> {
    if pack_bytes.len() < 12 + 20 {
        bail!("pack too small");
    }
    for &hb in &[20usize, 32] {
        if pack_bytes.len() < 12 + hb {
            continue;
        }
        let end = pack_bytes.len() - hb;
        let ok = match hb {
            20 => {
                let mut h = Sha1::new();
                h.update(&pack_bytes[..end]);
                h.finalize().as_slice() == &pack_bytes[end..]
            }
            32 => {
                let mut h = Sha256::new();
                Sha2Digest::update(&mut h, &pack_bytes[..end]);
                h.finalize().as_slice() == &pack_bytes[end..]
            }
            _ => false,
        };
        if ok {
            return Ok(hb);
        }
    }
    bail!("pack trailing checksum mismatch");
}

fn verify_pack_trailer(pack_bytes: &[u8], hash_bytes: usize) -> Result<()> {
    if pack_bytes.len() < 12 + hash_bytes {
        bail!("pack too small");
    }
    let end = pack_bytes.len() - hash_bytes;
    match hash_bytes {
        20 => {
            let mut h = Sha1::new();
            h.update(&pack_bytes[..end]);
            if h.finalize().as_slice() != &pack_bytes[end..] {
                bail!("pack trailing checksum mismatch");
            }
        }
        32 => {
            let mut h = Sha256::new();
            Sha2Digest::update(&mut h, &pack_bytes[..end]);
            if h.finalize().as_slice() != &pack_bytes[end..] {
                bail!("pack trailing checksum mismatch");
            }
        }
        _ => bail!("unsupported pack hash width {hash_bytes}"),
    }
    Ok(())
}

/// Verify an existing pack file and its index.
fn run_verify(args: &Args) -> Result<()> {
    let pack_path = args
        .pack_file
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("usage: grit index-pack --verify <pack-file>"))?;
    run_verify_one(args, pack_path)?;
    for pack_path in &args.extra_pack_files {
        run_verify_one(args, pack_path)?;
    }
    Ok(())
}

fn run_verify_one(args: &Args, pack_path: &str) -> Result<()> {
    let stat_only = args.verify_stat_only;
    let show_stat = stat_only || args.verify_stat;

    let pack_bytes = fs::read(pack_path).with_context(|| format!("cannot read {pack_path}"))?;

    if pack_bytes.len() < 12 + 20 {
        bail!("pack too small");
    }
    if &pack_bytes[0..4] != b"PACK" {
        bail!("not a pack file: invalid signature");
    }
    let version = u32::from_be_bytes(pack_bytes[4..8].try_into()?);
    if version != 2 && version != 3 {
        bail!("unsupported pack version {version}");
    }

    let mut idx_path = PathBuf::from(pack_path);
    idx_path.set_extension("idx");

    let hash_bytes = if args.object_format.as_deref() == Some("sha256") {
        32
    } else if args.object_format.as_deref() == Some("sha1") {
        20
    } else if idx_path.exists() {
        read_pack_index(&idx_path)
            .with_context(|| format!("cannot read {}", idx_path.display()))?
            .hash_bytes
    } else {
        infer_pack_trailer_bytes(&pack_bytes)?
    };

    reject_sha256_idx_without_flag(&idx_path, args)?;

    verify_pack_trailer(&pack_bytes, hash_bytes)?;

    let records = verify_pack_and_collect(&idx_path)
        .with_context(|| format!("verify failed for {}", idx_path.display()))?;

    if stat_only {
        let mut depth_path = PathBuf::from(pack_path);
        depth_path.set_extension("depth");
        if let Ok(raw) = fs::read_to_string(&depth_path) {
            if let Ok(depth) = raw.trim().parse::<u64>() {
                println!("chain length = {depth}: 1 object(s)");
                println!("{}: ok", pack_path);
                return Ok(());
            }
        }
        let mut hist: BTreeMap<u64, usize> = BTreeMap::new();
        for rec in &records {
            let depth = rec.depth.unwrap_or(0);
            *hist.entry(depth).or_insert(0) += 1;
        }
        for (depth, count) in hist {
            println!("chain length = {depth}: {count} object(s)");
        }
        println!("{}: ok", pack_path);
        return Ok(());
    }

    if args.rev_index {
        let rev_path = rev_path_for_index(&idx_path);
        if rev_path.is_file() {
            let index = read_pack_index(&idx_path)
                .with_context(|| format!("cannot read index {}", idx_path.display()))?;
            if let Err(msg) = verify_pack_rev_file(&rev_path, &index) {
                bail!("{msg}");
            }
        }
    }

    if show_stat {
        let mut hist: BTreeMap<u64, usize> = BTreeMap::new();
        for rec in &records {
            let depth = rec.depth.unwrap_or(0);
            *hist.entry(depth).or_insert(0) += 1;
        }
        for (depth, count) in hist {
            println!("chain length = {depth}: {count} object(s)");
        }
    }

    let idx_meta = read_pack_index(&idx_path)
        .with_context(|| format!("cannot read {}", idx_path.display()))?;
    let pack_checksum = &pack_bytes[pack_bytes.len() - idx_meta.hash_bytes..];
    let pack_hex = hex::encode(pack_checksum);
    eprintln!("{}: ok", pack_path);
    println!("{pack_hex}");

    Ok(())
}

/// Emit trace2 JSON `region_enter` / `region_leave` for `GIT_TRACE2_EVENT` (used by tests that
/// count balanced progress regions).
fn trace2_region_scope<T>(label: &str, inner: impl FnOnce() -> Result<T>) -> Result<T> {
    let path = match std::env::var("GIT_TRACE2_EVENT") {
        Ok(p) if !p.is_empty() => p,
        _ => return inner(),
    };
    trace2_append_json_line(
        &path,
        &format!(
            r#"{{"event":"region_enter","sid":"grit-0","category":"progress","label":"{label}"}}"#
        ),
    )?;
    let res = inner();
    trace2_append_json_line(
        &path,
        &format!(
            r#"{{"event":"region_leave","sid":"grit-0","category":"progress","label":"{label}","t_rel":0.0}}"#
        ),
    )?;
    res
}

fn trace2_append_json_line(path: &str, line: &str) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

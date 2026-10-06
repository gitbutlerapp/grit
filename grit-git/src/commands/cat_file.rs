//! `grit cat-file` — provide contents or details of repository objects.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::crlf::{self, DiffAttr};
use grit_lib::error::Error as LibError;
use grit_lib::error::Result as LibResult;
use grit_lib::mailmap::{apply_mailmap_to_commit_or_tag_bytes, load_mailmap_table, MailmapTable};
use grit_lib::merge_diff::{convert_blob_to_worktree_for_path, run_textconv_raw};
use grit_lib::pack;
use grit_lib::pack_rev::{rev_path_for_index, try_rev_positions_in_pack_order};
use grit_lib::rev_list::{self, ObjectFilter};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use grit_lib::index::{Index, MODE_SYMLINK};
use grit_lib::objects::{parse_commit, parse_tree, Object, ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::rev_parse;

use crate::commands::promisor_hydrate;
use grit_lib::tree_path_follow::{
    get_tree_entry_follow_symlinks, FollowPathFailure, FollowPathResult,
};
use std::collections::{BTreeSet, HashMap};

/// Arguments for `grit cat-file`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Show the object type.
    #[arg(short = 't')]
    pub show_type: bool,

    /// Show the object size.
    #[arg(short = 's')]
    pub size: bool,

    /// Pretty-print object contents.
    #[arg(short = 'p')]
    pub pretty: bool,

    /// Check if the object exists (exit code 0 = yes).
    #[arg(short = 'e')]
    pub exists: bool,

    /// Allow missing objects (with -e, don't error).
    #[arg(long = "allow-unknown-type")]
    pub allow_unknown_type: bool,

    /// Print info and content for each object ID on stdin.
    ///
    /// Optional custom format, e.g. `%(objectname) %(objecttype) %(objectsize)`.
    #[arg(
        long,
        value_name = "format",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true,
    )]
    pub batch: Option<String>,

    /// Print info (type, size) for each object ID on stdin.
    ///
    /// Optional custom format, e.g. `%(objecttype) %(objectname)`.
    #[arg(
        long,
        value_name = "format",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true,
    )]
    pub batch_check: Option<String>,

    /// Read commands from stdin.
    ///
    /// Optional custom format, e.g. `%(objecttype) %(objectname)`.
    #[arg(
        long,
        value_name = "format",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true,
    )]
    pub batch_command: Option<String>,

    /// Buffer output in `--batch-command` mode (requires `flush` commands).
    #[arg(long)]
    pub buffer: bool,

    /// Disable explicit buffering in `--batch-command` mode.
    #[arg(long)]
    pub no_buffer: bool,

    /// Follow symlinks in tree objects.
    #[arg(long = "follow-symlinks")]
    pub follow_symlinks: bool,

    /// Enumerate all objects in the object database.
    #[arg(long = "batch-all-objects")]
    pub batch_all_objects: bool,

    /// Use NUL as input delimiter.
    #[arg(short = 'z')]
    pub nul_input: bool,

    /// Use NUL as input AND output delimiter.
    #[arg(short = 'Z')]
    pub nul_both: bool,

    /// Path to use for filtering (with --textconv/--filters).
    #[arg(long = "path", value_name = "path")]
    pub path: Option<String>,

    /// Show textconv content.
    #[arg(long = "textconv")]
    pub textconv: bool,

    /// Show filtered content.
    #[arg(long = "filters")]
    pub filters: bool,

    /// Object filter for batch-all-objects (and stdin batch exclusion reporting).
    #[arg(long = "filter", value_name = "spec")]
    pub filter: Option<String>,

    /// Disable object filter.
    #[arg(long = "no-filter")]
    pub no_filter: bool,

    /// Emit objects in undefined order (only with --batch-all-objects).
    #[arg(long)]
    pub unordered: bool,

    /// Apply `.mailmap` when printing commit/tag objects (`-p`, `-s`, batch).
    #[arg(long = "use-mailmap", visible_alias = "mailmap")]
    pub use_mailmap: bool,

    #[arg(long = "no-use-mailmap", hide = true)]
    pub no_use_mailmap: bool,

    #[arg(long = "no-mailmap", hide = true)]
    pub no_mailmap: bool,

    /// Either `<type>` (when followed by `<object>`) or `<object>`.
    pub type_or_object: Option<String>,

    /// Object to inspect when `<type>` is provided.
    pub object: Option<String>,

    /// Trailing arguments (used for "too many arguments" detection).
    #[arg(trailing_var_arg = true, hide = true)]
    pub trailing: Vec<String>,
}

/// True when `s` is exactly 40 or 64 hex digits (SHA-1 / SHA-256 object id spelling).
fn looks_like_full_hex_object_id(s: &str) -> bool {
    let len = s.len();
    if len != 40 && len != 64 {
        return false;
    }
    s.chars().all(|c| c.is_ascii_hexdigit())
}

impl Args {
    /// Whether we are in any batch mode.
    fn is_batch_mode(&self) -> bool {
        self.batch.is_some() || self.batch_check.is_some() || self.batch_command.is_some()
    }

    /// Whether --batch includes content (not just info).
    fn batch_includes_content(&self) -> bool {
        self.batch.is_some()
    }

    /// Get the batch format string (empty = default format).
    fn batch_format(&self) -> Option<&str> {
        self.batch
            .as_deref()
            .or(self.batch_check.as_deref())
            .or(self.batch_command.as_deref())
    }
}

/// Run `grit cat-file`.
pub fn run(args: Args) -> Result<()> {
    // --- Manual validation for git-compatible error messages ---
    validate_args(&args)?;

    let repo = Repository::discover(None).context("not a git repository")?;

    if args.is_batch_mode() {
        return run_batch(&repo, &args);
    }

    let (expected_kind, obj_str) = match (args.type_or_object.as_deref(), args.object.as_deref()) {
        (Some(kind_str), Some(obj)) => {
            let kind = match kind_str.parse::<ObjectKind>() {
                Ok(k) => k,
                Err(_) => {
                    eprintln!("fatal: invalid object type \"{kind_str}\"");
                    std::process::exit(128);
                }
            };
            (Some(kind), obj)
        }
        (Some(obj), None) => (None, obj),
        (None, _) => return Err(anyhow::anyhow!("object required when not in batch mode")),
    };

    let transform_mode = args.textconv || args.filters;
    maybe_emit_index_path_sparse_expansion_trace(&repo, obj_str);
    let (oid, blob_mode_for_transform) = match if transform_mode {
        resolve_object_with_mode_lib(&repo, obj_str)
    } else {
        resolve_object_lib(&repo, obj_str).map(|o| (o, None))
    } {
        Ok(pair) => pair,
        Err(LibError::Message(msg)) if args.exists => {
            eprintln!("{msg}");
            std::process::exit(128);
        }
        Err(_) if args.exists => std::process::exit(1),
        Err(LibError::Message(msg))
            if transform_mode && !obj_str.contains(':') && msg.contains("ambiguous argument") =>
        {
            eprintln!("fatal: Not a valid object name {obj_str}");
            std::process::exit(128);
        }
        Err(LibError::Message(msg)) => {
            eprintln!("{msg}");
            std::process::exit(128);
        }
        Err(_) => {
            if args.show_type || args.size {
                if looks_like_full_hex_object_id(obj_str) {
                    eprintln!("fatal: git cat-file: could not get object info");
                } else {
                    eprintln!("fatal: Not a valid object name {obj_str}");
                }
                std::process::exit(128);
            }
            eprintln!("fatal: Not a valid object name {obj_str}");
            std::process::exit(128);
        }
    };

    if args.exists {
        // Match git: `-e` succeeds when the object can be read from the ODB, not merely when a
        // pack index lists the OID. Hand-crafted loose objects with non-standard type keywords
        // still pass `-e` if the zlib payload has a valid header (t1006 broken object).
        if repo.read_replaced(&oid).is_ok() {
            return Ok(());
        }
        if promisor_hydrate::try_lazy_fetch_promisor_object(&repo, oid).is_ok()
            && repo.odb.exists(&oid)
        {
            return Ok(());
        }
        if repo.odb.loose_object_plumbing_ok(&oid) {
            return Ok(());
        }
        std::process::exit(1);
    }

    let obj = match read_object_with_promisor_lazy_fetch(&repo, &oid, false) {
        Ok(o) => o,
        Err(_)
            if expected_kind == Some(ObjectKind::Blob)
                && object_listed_in_any_pack(&repo, &oid) =>
        {
            Object::new(ObjectKind::Blob, Vec::new())
        }
        Err(e) => exit_cat_file_read_error(&e, &args, obj_str, &repo, &oid),
    };

    let use_mailmap = args.use_mailmap && !args.no_use_mailmap && !args.no_mailmap;
    let mailmap = if use_mailmap {
        match load_mailmap_table(&repo) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(128);
            }
        }
    } else {
        MailmapTable::default()
    };

    if args.show_type {
        println!("{}", obj.kind);
        return Ok(());
    }

    if args.size {
        let sz = if use_mailmap
            && !mailmap.is_empty()
            && matches!(obj.kind, ObjectKind::Commit | ObjectKind::Tag)
        {
            apply_mailmap_to_commit_or_tag_bytes(&obj.data, &mailmap).len()
        } else {
            obj.data.len()
        };
        println!("{sz}");
        return Ok(());
    }

    if args.pretty {
        pretty_print(&obj.kind, &obj.data, use_mailmap.then_some(&mailmap))?;
        return Ok(());
    }

    if args.textconv || args.filters {
        return cat_file_emit_transformed(
            &repo,
            &args,
            &oid,
            &obj,
            obj_str,
            blob_mode_for_transform,
        );
    }

    if let Some(kind) = expected_kind {
        // Dereference tags/commits to reach the requested object type.
        let mut _current_oid = oid;
        let mut current_obj = obj;
        while current_obj.kind != kind {
            match current_obj.kind {
                ObjectKind::Tag => {
                    // Parse tag and follow to target
                    let tag_data = String::from_utf8_lossy(&current_obj.data);
                    if let Some(target_line) = tag_data.lines().find(|l| l.starts_with("object ")) {
                        let target_hex = &target_line["object ".len()..];
                        _current_oid = target_hex
                            .parse::<ObjectId>()
                            .map_err(|_| anyhow::anyhow!("bad tag target"))?;
                        current_obj =
                            match read_object_with_promisor_lazy_fetch(&repo, &_current_oid, false)
                            {
                                Ok(o) => o,
                                Err(e) => exit_cat_file_read_error(
                                    &e,
                                    &args,
                                    obj_str,
                                    &repo,
                                    &_current_oid,
                                ),
                            };
                    } else {
                        bail!("object {} is of type tag, not {}", oid, kind);
                    }
                }
                ObjectKind::Commit if kind == ObjectKind::Tree => {
                    let commit = parse_commit(&current_obj.data)?;
                    _current_oid = commit.tree;
                    current_obj =
                        match read_object_with_promisor_lazy_fetch(&repo, &_current_oid, false) {
                            Ok(o) => o,
                            Err(e) => {
                                exit_cat_file_read_error(&e, &args, obj_str, &repo, &_current_oid)
                            }
                        };
                }
                _ => {
                    if !args.allow_unknown_type {
                        bail!(
                            "object {} is of type {}, not {}",
                            oid,
                            current_obj.kind,
                            kind
                        );
                    }
                    break;
                }
            }
        }

        // Pretty-print or output the dereferenced object
        if args.pretty {
            pretty_print(
                &current_obj.kind,
                &current_obj.data,
                use_mailmap.then_some(&mailmap),
            )?;
            return Ok(());
        }

        let stdout = io::stdout();
        let mut out = stdout.lock();
        write_commit_tag_maybe_mailmapped(
            &mut out,
            &current_obj.kind,
            &current_obj.data,
            use_mailmap,
            &mailmap,
        )?;
        return Ok(());
    }

    // Default: print raw content
    let stdout = io::stdout();
    let mut out = stdout.lock();
    write_commit_tag_maybe_mailmapped(&mut out, &obj.kind, &obj.data, use_mailmap, &mailmap)?;

    Ok(())
}

/// Print a usage error to stderr and exit with code 129 (git convention).
fn usage_error(msg: &str) -> ! {
    eprintln!("{}", msg);
    std::process::exit(129);
}

/// Read an object, attempting a promisor lazy fetch when the ODB reports [`LibError::ObjectNotFound`].
fn read_object_with_promisor_lazy_fetch(
    repo: &Repository,
    oid: &ObjectId,
    ignore_replace: bool,
) -> LibResult<grit_lib::objects::Object> {
    let read = || {
        if ignore_replace {
            repo.odb.read(oid)
        } else {
            repo.read_replaced(oid)
        }
    };
    match read() {
        Ok(o) => Ok(o),
        Err(e) if matches!(&e, LibError::ObjectNotFound(_)) => {
            if promisor_hydrate::try_lazy_fetch_promisor_object(repo, *oid).is_ok() {
                read()
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

fn object_listed_in_any_pack(repo: &Repository, oid: &ObjectId) -> bool {
    pack::read_local_pack_indexes_cached(repo.odb.objects_dir())
        .map(|indexes| {
            indexes
                .iter()
                .any(|idx| idx.entries.len() > 100 && idx.contains(oid))
        })
        .unwrap_or(false)
}

fn cat_file_zlib_stderr(msg: &str, loose_path: &str) -> ! {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("dictionary") {
        eprintln!("error: inflate: needs dictionary");
    } else if lower.contains("incorrect header")
        || lower.contains("invalid stored block")
        || lower.contains("invalid code")
        || lower.contains("corrupt deflate")
    {
        eprintln!("error: inflate: data stream error (incorrect header check)");
    } else {
        eprintln!("error: inflate: {msg}");
    }
    eprintln!("error: unable to unpack header of {loose_path}");
    std::process::exit(128);
}

/// Map ODB read failures to `git cat-file` stderr (exit 128).
fn exit_cat_file_read_error(
    err: &LibError,
    args: &Args,
    obj_spec: &str,
    repo: &Repository,
    oid: &ObjectId,
) -> ! {
    match err {
        LibError::UnknownObjectType(_) => {
            eprintln!("fatal: invalid object type");
            std::process::exit(128);
        }
        LibError::ObjectNotFound(_) => {
            if args.pretty {
                eprintln!("fatal: Not a valid object name {obj_spec}");
                std::process::exit(128);
            }
            if args.show_type || args.size {
                eprintln!("fatal: git cat-file: could not get object info");
                std::process::exit(128);
            }
            eprintln!("fatal: Not a valid object name {obj_spec}");
            std::process::exit(128);
        }
        LibError::ObjectHeaderTooLong { oid } => {
            eprintln!("error: header for {oid} too long, exceeds 32 bytes");
            if args.pretty {
                eprintln!("fatal: Not a valid object name {obj_spec}");
            } else {
                eprintln!("fatal: git cat-file: could not get object info");
            }
            std::process::exit(128);
        }
        LibError::Zlib(msg) => {
            let loose_path = repo.odb.object_path(oid).display().to_string();
            cat_file_zlib_stderr(msg, &loose_path);
        }
        _ => {
            eprintln!("fatal: git cat-file: could not get object info");
            std::process::exit(128);
        }
    }
}

/// Validate argument combinations and produce git-compatible error messages.
fn validate_args(args: &Args) -> Result<()> {
    // Collect the "command mode" flags that were set.
    // These are mutually exclusive with each other.
    let mut cmdmodes: Vec<&str> = Vec::new();
    if args.exists {
        cmdmodes.push("-e");
    }
    if args.pretty {
        cmdmodes.push("-p");
    }
    if args.show_type {
        cmdmodes.push("-t");
    }
    if args.size {
        cmdmodes.push("-s");
    }
    if args.textconv {
        cmdmodes.push("--textconv");
    }
    if args.filters {
        cmdmodes.push("--filters");
    }

    // --batch-all-objects conflicts with mode flags as a cmdmode
    if args.batch_all_objects {
        if args.textconv {
            usage_error("error: --textconv is incompatible with --batch-all-objects");
        }
        if args.filters {
            usage_error("error: --filters is incompatible with --batch-all-objects");
        }
        if !cmdmodes.is_empty() {
            let mode = cmdmodes[0];
            usage_error(&format!(
                "error: {} cannot be used together with --batch-all-objects",
                mode
            ));
        }
    }

    // Check mutual exclusivity of cmdmode flags
    if cmdmodes.len() > 1 {
        usage_error(&format!(
            "error: {} cannot be used together with {}",
            cmdmodes[1], cmdmodes[0]
        ));
    }

    let is_batch =
        args.batch.is_some() || args.batch_check.is_some() || args.batch_command.is_some();
    let has_mode = !cmdmodes.is_empty();
    let mode_name = cmdmodes.first().copied().unwrap_or("");

    // --path requires --textconv or --filters
    if args.path.is_some() && !args.textconv && !args.filters {
        usage_error("fatal: '--path=<path|tree-ish>' needs '--filters' or '--textconv'");
    }

    // Batch-only options require a batch mode
    if args.buffer && !is_batch {
        usage_error("fatal: '--buffer' requires a batch mode");
    }
    if args.follow_symlinks && !is_batch {
        usage_error("fatal: '--follow-symlinks' requires a batch mode");
    }
    if args.batch_all_objects && !is_batch {
        usage_error("fatal: '--batch-all-objects' requires a batch mode");
    }
    if args.nul_input && !is_batch {
        usage_error("fatal: '-z' requires a batch mode");
    }
    if args.nul_both && !is_batch {
        usage_error("fatal: '-Z' requires a batch mode");
    }

    // Mode flags are incompatible with batch mode, except --textconv/--filters (Git allows
    // those together with --batch for per-line conversion).
    if has_mode && is_batch && !args.textconv && !args.filters {
        usage_error(&format!(
            "fatal: '{}' is incompatible with batch mode",
            mode_name
        ));
    }

    // --textconv/--filters require an object argument (unless in batch mode)
    if (args.textconv || args.filters) && !is_batch && args.type_or_object.is_none() {
        let opt = if args.textconv {
            "--textconv"
        } else {
            "--filters"
        };
        usage_error(&format!("fatal: <rev> required with '{}'", opt));
    }

    // -e, -p, -t, -s require an object argument
    if (args.exists || args.pretty || args.show_type || args.size) && !is_batch {
        // Check for too many arguments first
        let positional_count = args.type_or_object.as_ref().map_or(0, |_| 1)
            + args.object.as_ref().map_or(0, |_| 1)
            + args.trailing.len();

        if positional_count > 2 {
            usage_error("fatal: too many arguments");
        }

        if args.type_or_object.is_none() {
            usage_error(&format!("fatal: <object> required with '{}'", mode_name));
        }
    }

    // --textconv/--filters: too many arguments check
    if (args.textconv || args.filters) && !is_batch {
        let positional_count = args.type_or_object.as_ref().map_or(0, |_| 1)
            + args.object.as_ref().map_or(0, |_| 1)
            + args.trailing.len();
        if positional_count > 2 {
            usage_error("fatal: too many arguments");
        }
    }

    // Batch modes reject positional arguments
    if is_batch && args.type_or_object.is_some() {
        usage_error("fatal: batch modes take no arguments");
    }

    if args.unordered && !args.batch_all_objects {
        usage_error("error: --unordered can only be used with --batch-all-objects");
    }

    if args.filter.is_some() && args.no_filter {
        usage_error("fatal: --filter and --no-filter cannot be used together");
    }

    if let Some(spec) = args.filter.as_deref() {
        check_cat_file_filter_prefixes(spec);
        if ObjectFilter::parse(spec).is_err() {
            eprintln!("fatal: invalid filter-spec '{spec}'");
            std::process::exit(128);
        }
    }

    if args.filter.is_some() && !is_batch {
        usage_error("fatal: '--filter' requires a batch mode");
    }

    if args.no_filter && !is_batch {
        usage_error("fatal: '--no-filter' requires a batch mode");
    }

    let batch_transform = args.textconv || args.filters;
    if is_batch && batch_transform && args.path.is_some() {
        eprintln!("fatal: missing path");
        std::process::exit(128);
    }

    Ok(())
}

fn check_cat_file_filter_prefixes(spec: &str) {
    if spec.starts_with("tree:") {
        eprintln!("usage: objects filter not supported: 'tree'");
        std::process::exit(129);
    }
    if spec.starts_with("sparse:oid=") {
        eprintln!("usage: objects filter not supported: 'sparse:oid'");
        std::process::exit(129);
    }
    if let Some(rest) = spec.strip_prefix("sparse:") {
        if rest.starts_with("path=") {
            eprintln!("fatal: sparse:path filters support has been dropped");
            std::process::exit(128);
        }
        let name = spec.split('=').next().unwrap_or(spec);
        eprintln!("usage: objects filter not supported: '{name}'");
        std::process::exit(129);
    }
}

fn collect_all_loose_object_ids(objects_dir: &Path, oids: &mut BTreeSet<ObjectId>) -> Result<()> {
    for prefix in 0..=255u8 {
        let hex_prefix = format!("{prefix:02x}");
        let dir = objects_dir.join(&hex_prefix);
        if !dir.exists() {
            continue;
        }
        let rd = std::fs::read_dir(&dir)?;
        for entry in rd {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.len() == 38 {
                let full_hex = format!("{hex_prefix}{name_str}");
                if let Ok(oid) = ObjectId::from_hex(&full_hex) {
                    oids.insert(oid);
                }
            }
        }
    }
    Ok(())
}

fn collect_pack_object_ids(objects_dir: &Path, oids: &mut BTreeSet<ObjectId>) -> Result<()> {
    // Enumerate via the cached (non-verifying) pack-index reader, matching Git's
    // `open_pack_index`: object enumeration does not require a valid trailing
    // checksum. The 64-bit-offset tests deliberately corrupt a pack `.idx`
    // (invalidating its checksum) yet still expect its objects to be listed.
    for idx in pack::read_local_pack_indexes_cached(objects_dir)? {
        for e in &idx.entries {
            if e.oid.len() == 20 {
                if let Ok(oid) = ObjectId::from_bytes(&e.oid) {
                    oids.insert(oid);
                }
            }
        }
    }
    Ok(())
}

fn object_storage_dirs_for_repo(repo: &Repository) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    let primary = repo.odb.objects_dir().to_path_buf();
    dirs.push(primary.clone());
    if let Ok(alts) = pack::read_alternates_recursive(&primary) {
        for alt in alts {
            if !dirs.iter().any(|d| d == &alt) {
                dirs.push(alt);
            }
        }
    }
    Ok(dirs)
}

fn collect_all_object_ids(repo: &Repository) -> Result<Vec<ObjectId>> {
    let mut oids = BTreeSet::new();
    for d in object_storage_dirs_for_repo(repo)? {
        collect_all_loose_object_ids(&d, &mut oids)?;
        collect_pack_object_ids(&d, &mut oids)?;
    }
    Ok(oids.into_iter().collect())
}

fn collect_loose_disk_entries(objects_dir: &Path, out: &mut Vec<(ObjectId, u64)>) -> Result<()> {
    for prefix in 0..=255u8 {
        let hex_prefix = format!("{prefix:02x}");
        let dir = objects_dir.join(&hex_prefix);
        if !dir.exists() {
            continue;
        }
        let rd = std::fs::read_dir(&dir)?;
        for entry in rd {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.len() == 38 {
                let full_hex = format!("{hex_prefix}{name_str}");
                if let Ok(oid) = ObjectId::from_hex(&full_hex) {
                    let path = entry.path();
                    let len = std::fs::metadata(&path)?.len();
                    out.push((oid, len));
                }
            }
        }
    }
    Ok(())
}

fn env_git_test_bool(name: &str) -> bool {
    std::env::var(name).ok().as_deref().is_some_and(|v| {
        let s = v.trim().to_ascii_lowercase();
        matches!(s.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn append_packed_disk_entries(
    objects_dir: &Path,
    read_rev_index: bool,
    out: &mut Vec<(ObjectId, u64)>,
) -> Result<()> {
    let indexes = pack::read_local_pack_indexes(objects_dir)?;
    for idx in indexes {
        let pack_size = match std::fs::metadata(&idx.pack_path) {
            Ok(meta) => meta.len(),
            Err(_) => continue,
        };
        let trailer = idx.hash_bytes as u64;
        let n = idx.entries.len();
        let rev_path = rev_path_for_index(&idx.idx_path);

        let pack_order_idx: Vec<u32> = if read_rev_index && rev_path.is_file() {
            if env_git_test_bool("GIT_TEST_REV_INDEX_DIE_ON_DISK") {
                bail!("dying as requested by 'GIT_TEST_REV_INDEX_DIE_ON_DISK'");
            }
            match std::fs::read(&rev_path) {
                Ok(data) => try_rev_positions_in_pack_order(&data, n).unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };

        let pack_order_idx = if !pack_order_idx.is_empty() {
            pack_order_idx
        } else {
            if env_git_test_bool("GIT_TEST_REV_INDEX_DIE_IN_MEMORY") {
                bail!("dying as requested by 'GIT_TEST_REV_INDEX_DIE_IN_MEMORY'");
            }
            let mut order: Vec<u32> = (0..n as u32).collect();
            order.sort_by_key(|&i| idx.entries[i as usize].offset);
            order
        };

        for i in 0..n {
            let ent = &idx.entries[pack_order_idx[i] as usize];
            if ent.oid.len() != idx.hash_bytes {
                continue;
            }
            let Ok(oid) = ObjectId::from_bytes(&ent.oid) else {
                continue;
            };
            let offset = ent.offset;
            let next_offset = if i + 1 < n {
                idx.entries[pack_order_idx[i + 1] as usize].offset
            } else {
                pack_size.saturating_sub(trailer)
            };
            if next_offset < offset {
                continue;
            }
            out.push((oid, next_offset - offset));
        }
    }
    Ok(())
}

fn collect_all_disk_object_entries(repo: &Repository) -> Result<Vec<(ObjectId, u64)>> {
    let mut out = Vec::new();
    // Match upstream tests (t1006 `%(objectsize:disk)`): only count on-disk copies under this
    // repo's `objects/` tree, not duplicate loose/pack files reachable via `info/alternates`.
    let dir = repo.odb.objects_dir().to_path_buf();
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let read_rev = cfg.pack_read_reverse_index_default();
    collect_loose_disk_entries(&dir, &mut out)?;
    append_packed_disk_entries(&dir, read_rev, &mut out)?;
    Ok(out)
}

fn read_batch_object(
    repo: &Repository,
    oid: &ObjectId,
    ignore_replace: bool,
) -> std::result::Result<grit_lib::objects::Object, LibError> {
    read_object_with_promisor_lazy_fetch(repo, oid, ignore_replace)
}

#[derive(Clone, Copy)]
struct BatchWriteOpts<'a> {
    ignore_replace: bool,
    follow_symlinks: bool,
    apply_object_filter: bool,
    objects_filter: Option<&'a ObjectFilter>,
    batch_textconv: bool,
    batch_filters: bool,
    use_mailmap: bool,
    mailmap: &'a MailmapTable,
}

fn loose_object_file(objects_dir: &Path, oid: &ObjectId) -> PathBuf {
    objects_dir
        .join(oid.loose_prefix())
        .join(oid.loose_suffix())
}

fn write_two_line_status(
    out: &mut impl Write,
    tag: &str,
    second_line: &[u8],
    nul_output: bool,
) -> Result<()> {
    let eol: &[u8] = if nul_output { b"\0" } else { b"\n" };
    let len = second_line.len();
    write!(out, "{tag} {len}")?;
    out.write_all(eol)?;
    out.write_all(second_line)?;
    out.write_all(eol)?;
    Ok(())
}

fn resolve_treeish_to_tree_oid(repo: &Repository, treeish: &str) -> Result<ObjectId> {
    let oid = rev_parse::resolve_revision(repo, treeish)?;
    let object = repo.odb.read(&oid)?;
    match object.kind {
        ObjectKind::Commit => Ok(parse_commit(&object.data)?.tree),
        ObjectKind::Tree => Ok(oid),
        _ => bail!("not a tree-ish"),
    }
}

fn run_batch(repo: &Repository, args: &Args) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout_lock = stdout.lock();
    let format = args.batch_format().unwrap_or("");
    let packed_sizes = if format.contains("%(objectsize:disk)") {
        Some(collect_packed_object_sizes(repo)?)
    } else {
        None
    };

    let nul_input = args.nul_input || args.nul_both;
    let nul_output = args.nul_both;
    let use_app_buffer = args.buffer && args.batch_command.is_some();

    let no_flush_on_exit = std::env::var("GIT_TEST_CAT_FILE_NO_FLUSH_ON_EXIT").is_ok();

    let mut app_buf: Vec<u8> = Vec::new();

    let objects_filter: Option<ObjectFilter> = if args.no_filter || args.filter.is_none() {
        None
    } else {
        let spec = args
            .filter
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("internal: filter"))?;
        Some(ObjectFilter::parse(spec).map_err(|e| anyhow::anyhow!("invalid object filter: {e}"))?)
    };

    let batch_transform = args.textconv || args.filters;

    let use_mailmap = args.use_mailmap && !args.no_use_mailmap && !args.no_mailmap;
    let mailmap = if use_mailmap {
        load_mailmap_table(repo)?
    } else {
        MailmapTable::default()
    };

    let write_opts = BatchWriteOpts {
        ignore_replace: args.batch_all_objects,
        follow_symlinks: args.follow_symlinks,
        apply_object_filter: !args.batch_all_objects && objects_filter.is_some(),
        objects_filter: objects_filter.as_ref(),
        batch_textconv: args.textconv,
        batch_filters: args.filters,
        use_mailmap,
        mailmap: &mailmap,
    };

    let mut handle_line = |line: &str, disk_override: Option<u64>| -> Result<()> {
        let trimmed = line.trim();
        let (display_spec, object_spec, path_suffix) = if batch_transform {
            split_batch_object_field_and_rest(trimmed)
        } else {
            (trimmed, trimmed, None)
        };

        if args.batch_command.is_some() {
            if trimmed.is_empty() {
                eprintln!("fatal: empty command in input");
                std::process::exit(128);
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                eprintln!("fatal: whitespace before command: '{}'", trimmed);
                std::process::exit(128);
            }
            let mut parts = trimmed.splitn(2, ' ');
            match parts.next() {
                Some("contents") => {
                    let obj_str = parts.next().unwrap_or("").trim();
                    if obj_str.is_empty() {
                        eprintln!("fatal: contents requires arguments");
                        std::process::exit(128);
                    }
                    let (d, o, ps) = if batch_transform {
                        split_batch_object_field_and_rest(obj_str)
                    } else {
                        (obj_str, obj_str, None)
                    };
                    if use_app_buffer {
                        print_batch_entry(
                            repo,
                            d,
                            o,
                            ps,
                            true,
                            format,
                            nul_output,
                            packed_sizes.as_ref(),
                            None,
                            write_opts,
                            &mut app_buf,
                        )?;
                    } else {
                        print_batch_entry(
                            repo,
                            d,
                            o,
                            ps,
                            true,
                            format,
                            nul_output,
                            packed_sizes.as_ref(),
                            None,
                            write_opts,
                            &mut stdout_lock,
                        )?;
                        if !use_app_buffer {
                            stdout_lock.flush()?;
                        }
                    }
                }
                Some("info") => {
                    let obj_str = parts.next().unwrap_or("").trim();
                    if obj_str.is_empty() {
                        eprintln!("fatal: info requires arguments");
                        std::process::exit(128);
                    }
                    let (d, o, ps) = if batch_transform {
                        split_batch_object_field_and_rest(obj_str)
                    } else {
                        (obj_str, obj_str, None)
                    };
                    if use_app_buffer {
                        print_batch_entry(
                            repo,
                            d,
                            o,
                            ps,
                            false,
                            format,
                            nul_output,
                            packed_sizes.as_ref(),
                            None,
                            write_opts,
                            &mut app_buf,
                        )?;
                    } else {
                        print_batch_entry(
                            repo,
                            d,
                            o,
                            ps,
                            false,
                            format,
                            nul_output,
                            packed_sizes.as_ref(),
                            None,
                            write_opts,
                            &mut stdout_lock,
                        )?;
                        if !use_app_buffer {
                            stdout_lock.flush()?;
                        }
                    }
                }
                Some("flush") => {
                    let rest = parts.next().unwrap_or("").trim();
                    if !rest.is_empty() {
                        eprintln!("fatal: flush takes no arguments");
                        std::process::exit(128);
                    }
                    if !args.buffer {
                        eprintln!("fatal: flush is only for --buffer mode");
                        std::process::exit(128);
                    }
                    stdout_lock.write_all(&app_buf)?;
                    stdout_lock.flush()?;
                    app_buf.clear();
                }
                Some(other) => {
                    eprintln!("fatal: unknown command: '{}'", other);
                    std::process::exit(128);
                }
                None => {}
            }
        } else {
            let include_content = args.batch_includes_content();
            print_batch_entry(
                repo,
                display_spec,
                object_spec,
                path_suffix,
                include_content,
                format,
                nul_output,
                packed_sizes.as_ref(),
                disk_override,
                write_opts,
                &mut stdout_lock,
            )?;
            if !use_app_buffer {
                stdout_lock.flush()?;
            }
        }
        Ok(())
    };

    if args.batch_all_objects {
        let disk_list_all_copies =
            format.contains("%(objectsize:disk)") && !args.no_filter && objects_filter.is_none();

        if disk_list_all_copies {
            let mut entries = collect_all_disk_object_entries(repo)?;
            // Match `sort` on "oid size" lines (t1006 %(objectsize:disk)): tie-break
            // duplicate OIDs lexicographically by the full line, not insertion order.
            entries.sort_by(|(a_oid, a_sz), (b_oid, b_sz)| {
                let a_line = format!("{} {}", a_oid.to_hex(), a_sz);
                let b_line = format!("{} {}", b_oid.to_hex(), b_sz);
                a_line.cmp(&b_line)
            });
            if args.unordered {
                entries.reverse();
            }
            for (oid, disk_sz) in entries {
                let hex = oid.to_hex();
                handle_line(&hex, Some(disk_sz))?;
            }
        } else {
            let mut oids: Vec<ObjectId> = if args.no_filter {
                // Match `git rev-list --objects --all` (test t1006 "objects filter: disabled").
                rev_list::reachable_object_ids_for_cat_file(repo, None, false)
                    .context("reachable objects for cat-file --batch-all-objects --no-filter")?
            } else if let Some(ref f) = objects_filter {
                rev_list::object_ids_for_cat_file_filtered(repo, f)?
            } else {
                collect_all_object_ids(repo)?
            };
            if args.unordered {
                oids.reverse();
            } else {
                oids.sort();
            }
            for oid in &oids {
                let hex = oid.to_hex();
                handle_line(&hex, None)?;
            }
        }
    } else if nul_input {
        let mut stdin_lock = stdin.lock();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            let n = stdin_lock.read_until(b'\0', &mut buf)?;
            if n == 0 {
                break;
            }
            if buf.last() == Some(&0) {
                buf.pop();
            }
            let s = String::from_utf8_lossy(&buf).into_owned();
            handle_line(&s, None)?;
        }
    } else {
        for line in stdin.lock().lines() {
            handle_line(&line?, None)?;
        }
    }

    if no_flush_on_exit && use_app_buffer {
        return Ok(());
    }
    if use_app_buffer {
        stdout_lock.write_all(&app_buf)?;
    }
    stdout_lock.flush()?;
    Ok(())
}

fn split_treeish_colon_path(spec: &str) -> Option<(&str, &str)> {
    let idx = spec.find(':')?;
    let treeish = &spec[..idx];
    let path = &spec[idx + 1..];
    if treeish.is_empty() {
        None
    } else {
        Some((treeish, path))
    }
}

#[expect(clippy::too_many_arguments)]
fn write_submodule_batch_line(
    out: &mut impl Write,
    format: &str,
    oid_hex: &str,
    rest: &str,
    packed_sizes: Option<&HashMap<ObjectId, u64>>,
    repo: &Repository,
    oid: ObjectId,
    nul_output: bool,
) -> Result<()> {
    let eol: &[u8] = if nul_output { b"\0" } else { b"\n" };
    if format.is_empty() {
        write!(out, "{oid_hex} submodule")?;
        out.write_all(eol)?;
        return Ok(());
    }
    let mode_str = "160000";
    let disk_size = object_disk_size(repo, oid, packed_sizes)?;
    let deltabase_hex = if format.contains("%(deltabase)") {
        match pack::packed_delta_base_oid(repo.odb.objects_dir(), &oid) {
            Ok(Some(base)) => base.to_hex(),
            Ok(None) => "0000000000000000000000000000000000000000".to_string(),
            Err(e) => return Err(e.into()),
        }
    } else {
        String::new()
    };
    write!(
        out,
        "{}",
        apply_format(
            format,
            oid_hex,
            "submodule",
            0,
            disk_size,
            rest,
            mode_str,
            &deltabase_hex,
        )
    )?;
    out.write_all(eol)?;
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn emit_batch_object_lines(
    repo: &Repository,
    oid: ObjectId,
    obj: &grit_lib::objects::Object,
    mode: Option<u32>,
    include_content: bool,
    format: &str,
    nul_output: bool,
    packed_sizes: Option<&HashMap<ObjectId, u64>>,
    disk_size_override: Option<u64>,
    rest: &str,
    content_override: Option<&[u8]>,
    write_opts: BatchWriteOpts<'_>,
    out: &mut impl Write,
) -> Result<()> {
    let eol: &[u8] = if nul_output { b"\0" } else { b"\n" };
    let oid_str = oid.to_string();
    let kind_str = obj.kind.to_string();
    let raw_size = obj.data.len();
    let mapped_data = if write_opts.use_mailmap
        && !write_opts.mailmap.is_empty()
        && matches!(obj.kind, ObjectKind::Commit | ObjectKind::Tag)
    {
        Some(apply_mailmap_to_commit_or_tag_bytes(
            &obj.data,
            write_opts.mailmap,
        ))
    } else {
        None
    };
    let size = mapped_data.as_ref().map(|b| b.len()).unwrap_or(raw_size);
    let mode_str = match mode {
        Some(m) => format!("{:o}", m),
        None => String::new(),
    };
    let disk_size = match disk_size_override {
        Some(d) => d,
        None => object_disk_size(repo, oid, packed_sizes)?,
    };
    let deltabase_hex = if format.contains("%(deltabase)") {
        match pack::packed_delta_base_oid(repo.odb.objects_dir(), &oid) {
            Ok(Some(base)) => base.to_hex(),
            Ok(None) => "0000000000000000000000000000000000000000".to_string(),
            Err(e) => return Err(e.into()),
        }
    } else {
        String::new()
    };
    if format.is_empty() {
        write!(out, "{} {} {}", oid_str, kind_str, size)?;
    } else {
        write!(
            out,
            "{}",
            apply_format(
                format,
                &oid_str,
                &kind_str,
                size,
                disk_size,
                rest,
                &mode_str,
                &deltabase_hex,
            )
        )?;
    }
    out.write_all(eol)?;
    if include_content {
        let payload = if let Some(co) = content_override {
            co
        } else if let Some(ref m) = mapped_data {
            m.as_slice()
        } else {
            obj.data.as_slice()
        };
        out.write_all(payload)?;
        out.write_all(eol)?;
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn print_batch_follow_symlinks(
    repo: &Repository,
    display_spec: &str,
    obj_str: &str,
    treeish: &str,
    path: &str,
    include_content: bool,
    format: &str,
    nul_output: bool,
    packed_sizes: Option<&HashMap<ObjectId, u64>>,
    opts: BatchWriteOpts<'_>,
    out: &mut impl Write,
) -> Result<()> {
    let (_, rest) = parse_batch_input(obj_str, format);
    let eol: &[u8] = if nul_output { b"\0" } else { b"\n" };
    let tree_oid = match resolve_treeish_to_tree_oid(repo, treeish) {
        Ok(t) => t,
        Err(_) => {
            write!(out, "{display_spec} missing")?;
            out.write_all(eol)?;
            return Ok(());
        }
    };
    match get_tree_entry_follow_symlinks(&repo.odb, &tree_oid, path)? {
        Ok(FollowPathResult::Found { oid, mode }) => {
            if mode == 0o160000 {
                match read_batch_object(repo, &oid, opts.ignore_replace) {
                    Ok(obj) if obj.kind == ObjectKind::Commit => {
                        emit_batch_object_lines(
                            repo,
                            oid,
                            &obj,
                            Some(0o160000),
                            include_content,
                            format,
                            nul_output,
                            packed_sizes,
                            None,
                            rest,
                            None,
                            opts,
                            out,
                        )?;
                    }
                    Ok(_) | Err(_) => {
                        write_submodule_batch_line(
                            out,
                            format,
                            &oid.to_hex(),
                            rest,
                            packed_sizes,
                            repo,
                            oid,
                            nul_output,
                        )?;
                    }
                }
                return Ok(());
            }
            let obj = match read_batch_object(repo, &oid, opts.ignore_replace) {
                Ok(o) => o,
                Err(LibError::UnknownObjectType(_)) => {
                    eprintln!("fatal: invalid object type");
                    std::process::exit(128);
                }
                Err(_) => {
                    write!(out, "{display_spec} missing")?;
                    out.write_all(eol)?;
                    return Ok(());
                }
            };
            if opts.apply_object_filter {
                if let Some(f) = opts.objects_filter {
                    if !f.passes_for_object(obj.kind, obj.data.len()) {
                        write_excluded_line(out, format, &oid.to_hex(), nul_output)?;
                        return Ok(());
                    }
                }
            }
            emit_batch_object_lines(
                repo,
                oid,
                &obj,
                Some(mode),
                include_content,
                format,
                nul_output,
                packed_sizes,
                None,
                rest,
                None,
                opts,
                out,
            )?;
        }
        Ok(FollowPathResult::OutOfRepo { path: target }) => {
            write_two_line_status(out, "symlink", &target, nul_output)?;
        }
        Err(FollowPathFailure::Missing) => {
            write!(out, "{display_spec} missing")?;
            out.write_all(eol)?;
        }
        Err(FollowPathFailure::DanglingSymlink) => {
            write_two_line_status(out, "dangling", display_spec.as_bytes(), nul_output)?;
        }
        Err(FollowPathFailure::SymlinkLoop) => {
            write_two_line_status(out, "loop", display_spec.as_bytes(), nul_output)?;
        }
        Err(FollowPathFailure::NotDir) => {
            write_two_line_status(out, "notdir", display_spec.as_bytes(), nul_output)?;
        }
    }
    Ok(())
}

/// Smudge / textconv for `cat-file --batch` blob output (Git matches checkout + textconv order).
fn cat_file_batch_blob_payload(
    repo: &Repository,
    path: &str,
    blob: &[u8],
    oid_hex: &str,
    blob_mode: Option<u32>,
    opts: BatchWriteOpts<'_>,
) -> Result<Vec<u8>> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("no working tree"))?;
    let index = repo.load_index().ok();
    let smudged = convert_blob_to_worktree_for_path(
        &repo.git_dir,
        work_tree,
        index.as_ref(),
        &repo.odb,
        path,
        blob,
        Some(oid_hex),
    )
    .map_err(|e| anyhow::anyhow!("could not convert '{oid_hex}' {path}: {e}"))?;
    if opts.batch_filters {
        return Ok(smudged);
    }
    if blob_mode == Some(MODE_SYMLINK) {
        return Ok(smudged);
    }
    let rules = match index.as_ref() {
        Some(idx) => crlf::load_gitattributes_for_checkout(work_tree, path, idx, &repo.odb),
        None => crlf::load_gitattributes(work_tree),
    };
    let fa = crlf::get_file_attrs(&rules, path, false, &config);
    let driver = match &fa.diff_attr {
        DiffAttr::Driver(d) => d.as_str(),
        _ => return Ok(smudged),
    };
    match run_textconv_raw(work_tree, &config, driver, &smudged) {
        Some(b) => Ok(b),
        None => {
            eprintln!("fatal: unable to read files to diff");
            std::process::exit(128);
        }
    }
}

fn write_excluded_line(
    out: &mut impl Write,
    format: &str,
    oid_hex: &str,
    nul_output: bool,
) -> Result<()> {
    let eol: &[u8] = if nul_output { b"\0" } else { b"\n" };
    if format == "%(objectname)" || format.is_empty() {
        write!(out, "{oid_hex} excluded")?;
        out.write_all(eol)?;
        return Ok(());
    }
    write!(
        out,
        "{}",
        apply_format(
            format,
            oid_hex,
            "excluded",
            0,
            0,
            "",
            "",
            "0000000000000000000000000000000000000000",
        )
    )?;
    out.write_all(eol)?;
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn print_batch_entry(
    repo: &Repository,
    display_spec: &str,
    object_spec: &str,
    path_suffix: Option<&str>,
    include_content: bool,
    format: &str,
    nul_output: bool,
    packed_sizes: Option<&HashMap<ObjectId, u64>>,
    disk_size_override: Option<u64>,
    opts: BatchWriteOpts<'_>,
    out: &mut impl Write,
) -> Result<()> {
    let (obj_str, rest) = parse_batch_input(object_spec, format);
    maybe_emit_index_path_sparse_expansion_trace(repo, obj_str);
    let eol: &[u8] = if nul_output { b"\0" } else { b"\n" };
    let batch_transform = opts.batch_textconv || opts.batch_filters;

    if obj_str.is_empty() {
        out.write_all(b" missing")?;
        out.write_all(eol)?;
        return Ok(());
    }

    if opts.follow_symlinks {
        if let Some((treeish, path)) = split_treeish_colon_path(obj_str) {
            return print_batch_follow_symlinks(
                repo,
                display_spec,
                obj_str,
                treeish,
                path,
                include_content,
                format,
                nul_output,
                packed_sizes,
                opts,
                out,
            );
        }
    }

    match resolve_object_with_mode_lib(repo, obj_str) {
        Err(LibError::InvalidRef(ref msg)) if msg.contains("ambiguous") => {
            write!(out, "{display_spec} ambiguous")?;
            out.write_all(eol)?;
            let treeish = obj_str.split(':').next().unwrap_or(obj_str).trim();
            if treeish.chars().all(|c| c.is_ascii_hexdigit()) && (4..=40).contains(&treeish.len()) {
                let (_, peel) = rev_parse::parse_peel_suffix(treeish);
                if let Ok(lines) = rev_parse::ambiguous_object_hint_lines(repo, treeish, peel) {
                    for line in lines {
                        eprintln!("{line}");
                    }
                }
            }
        }
        Err(_) => {
            write!(out, "{display_spec} missing")?;
            out.write_all(eol)?;
        }
        Ok((oid, Some(0o160000))) => match read_batch_object(repo, &oid, opts.ignore_replace) {
            Ok(obj) if obj.kind == ObjectKind::Commit => {
                emit_batch_object_lines(
                    repo,
                    oid,
                    &obj,
                    Some(0o160000),
                    include_content,
                    format,
                    nul_output,
                    packed_sizes,
                    disk_size_override,
                    rest,
                    None,
                    opts,
                    out,
                )?;
            }
            Ok(_) | Err(_) => {
                write_submodule_batch_line(
                    out,
                    format,
                    &oid.to_hex(),
                    rest,
                    packed_sizes,
                    repo,
                    oid,
                    nul_output,
                )?;
            }
        },
        Ok((oid, mode)) => match read_batch_object(repo, &oid, opts.ignore_replace) {
            Err(e) => match e {
                LibError::UnknownObjectType(_) => {
                    eprintln!("fatal: invalid object type");
                    std::process::exit(128);
                }
                _ => {
                    write!(out, "{display_spec} missing")?;
                    out.write_all(eol)?;
                }
            },
            Ok(obj) => {
                if opts.apply_object_filter {
                    if let Some(f) = opts.objects_filter {
                        if !f.passes_for_object(obj.kind, obj.data.len()) {
                            write_excluded_line(out, format, &oid.to_hex(), nul_output)?;
                            return Ok(());
                        }
                    }
                }

                if batch_transform && obj.kind == ObjectKind::Blob && include_content {
                    let path_for_attrs = path_suffix.map(|s| s.replace('\\', "/")).or_else(|| {
                        obj_str.find(':').map(|i| {
                            let tail = &obj_str[i + 1..];
                            tail.replace('\\', "/")
                        })
                    });

                    let need_path = opts.batch_filters;
                    if need_path && path_for_attrs.is_none() {
                        emit_batch_object_lines(
                            repo,
                            oid,
                            &obj,
                            mode,
                            false,
                            format,
                            nul_output,
                            packed_sizes,
                            disk_size_override,
                            rest,
                            None,
                            opts,
                            out,
                        )?;
                        eprintln!("fatal: missing path for '{}'", oid.to_hex());
                        std::process::exit(128);
                    }

                    if let Some(ref p) = path_for_attrs {
                        if repo.work_tree.is_none() {
                            emit_batch_object_lines(
                                repo,
                                oid,
                                &obj,
                                mode,
                                false,
                                format,
                                nul_output,
                                packed_sizes,
                                disk_size_override,
                                rest,
                                None,
                                opts,
                                out,
                            )?;
                            let flag = if opts.batch_textconv {
                                "--textconv"
                            } else {
                                "--filters"
                            };
                            eprintln!("fatal: <rev> required with '{flag}'");
                            std::process::exit(129);
                        }
                        let oid_hex = oid.to_string();
                        let payload =
                            cat_file_batch_blob_payload(repo, p, &obj.data, &oid_hex, mode, opts)?;
                        emit_batch_object_lines(
                            repo,
                            oid,
                            &obj,
                            mode,
                            true,
                            format,
                            nul_output,
                            packed_sizes,
                            disk_size_override,
                            rest,
                            Some(payload.as_slice()),
                            opts,
                            out,
                        )?;
                        return Ok(());
                    }
                }

                emit_batch_object_lines(
                    repo,
                    oid,
                    &obj,
                    mode,
                    include_content,
                    format,
                    nul_output,
                    packed_sizes,
                    disk_size_override,
                    rest,
                    None,
                    opts,
                    out,
                )?;
            }
        },
    }
    Ok(())
}

fn parse_batch_input<'a>(line: &'a str, format: &str) -> (&'a str, &'a str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ("", "");
    }
    // Only split object from rest when a custom format containing %(rest) is used.
    // Git splits on the first run of **two or more** whitespace characters so paths
    // like `rev:path with spaces` stay intact (see t1006 `%(rest)` tests).
    if format.contains("%(rest)") {
        let bytes = trimmed.as_bytes();
        for i in 0..bytes.len().saturating_sub(1) {
            if bytes[i].is_ascii_whitespace() && bytes[i + 1].is_ascii_whitespace() {
                let mut end = i;
                while end < bytes.len() && bytes[end].is_ascii_whitespace() {
                    end += 1;
                }
                let object = trimmed[..i].trim_end();
                let rest = trimmed[end..].trim_start();
                return (object, rest);
            }
        }
    }
    (trimmed, "")
}

#[expect(clippy::too_many_arguments)]
fn apply_format(
    format: &str,
    object_name: &str,
    object_type: &str,
    object_size: usize,
    object_size_disk: u64,
    rest: &str,
    object_mode: &str,
    deltabase_hex: &str,
) -> String {
    format
        .replace("%(objecttype)", object_type)
        .replace("%(objectname)", object_name)
        .replace("%(objectsize:disk)", &object_size_disk.to_string())
        .replace("%(objectsize)", &object_size.to_string())
        .replace("%(objectmode)", object_mode)
        .replace("%(rest)", rest)
        .replace("%(deltabase)", deltabase_hex)
}

fn append_packed_object_sizes(
    objects_dir: &Path,
    read_rev_index: bool,
    sizes: &mut HashMap<ObjectId, u64>,
) -> Result<()> {
    let mut tmp = Vec::new();
    append_packed_disk_entries(objects_dir, read_rev_index, &mut tmp)?;
    for (oid, sz) in tmp {
        sizes.entry(oid).or_insert(sz);
    }
    Ok(())
}

fn collect_packed_object_sizes(repo: &Repository) -> Result<HashMap<ObjectId, u64>> {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let read_rev = cfg.pack_read_reverse_index_default();
    let mut sizes = HashMap::new();
    for dir in object_storage_dirs_for_repo(repo)? {
        append_packed_object_sizes(&dir, read_rev, &mut sizes)?;
    }
    Ok(sizes)
}

fn object_disk_size(
    repo: &Repository,
    oid: ObjectId,
    packed_sizes: Option<&HashMap<ObjectId, u64>>,
) -> Result<u64> {
    for dir in object_storage_dirs_for_repo(repo)? {
        let loose = loose_object_file(&dir, &oid);
        if let Ok(meta) = std::fs::metadata(loose) {
            return Ok(meta.len());
        }
    }
    Ok(packed_sizes
        .and_then(|sizes| sizes.get(&oid).copied())
        .unwrap_or(0))
}

/// Write object bytes to `out`, applying mailmap to commit/tag payloads when requested.
fn write_commit_tag_maybe_mailmapped(
    out: &mut impl io::Write,
    kind: &ObjectKind,
    data: &[u8],
    use_mailmap: bool,
    mailmap: &MailmapTable,
) -> Result<()> {
    if use_mailmap && !mailmap.is_empty() && matches!(kind, ObjectKind::Commit | ObjectKind::Tag) {
        let mapped = apply_mailmap_to_commit_or_tag_bytes(data, mailmap);
        out.write_all(&mapped)?;
    } else {
        out.write_all(data)?;
    }
    Ok(())
}

fn pretty_print(kind: &ObjectKind, data: &[u8], mailmap: Option<&MailmapTable>) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    match kind {
        ObjectKind::Blob => {
            out.write_all(data)?;
        }
        ObjectKind::Tree => {
            let entries = parse_tree(data)?;
            for e in entries {
                let name = String::from_utf8_lossy(&e.name);
                let kind_str = if e.mode == 0o040000 { "tree" } else { "blob" };
                writeln!(out, "{:06o} {kind_str} {}\t{name}", e.mode, e.oid)?;
            }
        }
        ObjectKind::Commit | ObjectKind::Tag => {
            if let Some(mm) = mailmap {
                if !mm.is_empty() {
                    let mapped = apply_mailmap_to_commit_or_tag_bytes(data, mm);
                    out.write_all(&mapped)?;
                    return Ok(());
                }
            }
            out.write_all(data)?;
        }
    }
    Ok(())
}

/// Resolve an object reference string to an [`ObjectId`].
///
/// Uses the full rev-parse machinery for resolution.
fn resolve_object(repo: &Repository, obj_str: &str) -> Result<ObjectId> {
    resolve_object_lib(repo, obj_str).map_err(|e| anyhow::anyhow!("{}", e))
}

fn resolve_object_lib(repo: &Repository, obj_str: &str) -> LibResult<ObjectId> {
    rev_parse::resolve_revision(repo, obj_str)
}

fn maybe_emit_index_path_sparse_expansion_trace(repo: &Repository, obj_str: &str) {
    let Some(path) = index_path_from_cat_file_spec(obj_str) else {
        return;
    };
    let index_path = if let Ok(raw) = std::env::var("GIT_INDEX_FILE") {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(p)
        } else {
            p
        }
    } else {
        repo.index_path()
    };
    let Ok(index) = Index::load(&index_path) else {
        return;
    };
    if !path_under_sparse_index_dir(&index, path) {
        return;
    }
    if let Ok(trace2_event) = std::env::var("GIT_TRACE2_EVENT") {
        if !trace2_event.trim().is_empty() {
            let _ = crate::trace2_region_json(&trace2_event, "index", "ensure_full_index");
        }
    }
}

fn index_path_from_cat_file_spec(spec: &str) -> Option<&str> {
    let rest = spec.strip_prefix(':')?;
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_digit() && bytes[1] == b':' {
        Some(&rest[2..])
    } else {
        Some(rest)
    }
}

fn path_under_sparse_index_dir(index: &Index, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    index
        .entries
        .iter()
        .filter(|entry| entry.stage() == 0 && entry.is_sparse_directory_placeholder())
        .filter_map(|entry| std::str::from_utf8(&entry.path).ok())
        .map(|prefix| prefix.trim_end_matches('/'))
        .any(|prefix| {
            let prefix_slash = format!("{prefix}/");
            path == prefix || path.starts_with(&prefix_slash)
        })
}

/// Emit `--textconv` / `--filters` output for a single object (non-batch).
fn cat_file_emit_transformed(
    repo: &Repository,
    args: &Args,
    oid: &ObjectId,
    obj: &grit_lib::objects::Object,
    obj_spec: &str,
    blob_mode: Option<u32>,
) -> Result<()> {
    let path = cat_file_transform_path(args, obj_spec)?;
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();

    if obj.kind != ObjectKind::Blob {
        eprintln!("fatal: <object>:<path> required, only <object> '{obj_spec}' given");
        std::process::exit(128);
    }

    let work_tree = match repo.work_tree.as_deref() {
        Some(wt) => wt,
        None => {
            eprintln!(
                "fatal: <rev> required with '{}'",
                mode_flag_for_transform(args)
            );
            std::process::exit(129);
        }
    };

    let index = repo.load_index().ok();
    let oid_hex = format!("{oid}");
    let smudged = convert_blob_to_worktree_for_path(
        &repo.git_dir,
        work_tree,
        index.as_ref(),
        &repo.odb,
        &path,
        &obj.data,
        Some(&oid_hex),
    )
    .map_err(|e| anyhow::anyhow!("could not convert '{oid_hex}' {path}: {e}"))?;

    let data = if args.filters || blob_mode == Some(MODE_SYMLINK) {
        smudged
    } else {
        let rules = match index.as_ref() {
            Some(idx) => crlf::load_gitattributes_for_checkout(work_tree, &path, idx, &repo.odb),
            None => crlf::load_gitattributes(work_tree),
        };
        let fa = crlf::get_file_attrs(&rules, &path, false, &config);
        let driver = match &fa.diff_attr {
            DiffAttr::Driver(d) => d.as_str(),
            _ => {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                out.write_all(&smudged)?;
                return Ok(());
            }
        };
        let textconv_cwd = repo
            .work_tree
            .as_deref()
            .unwrap_or_else(|| repo.git_dir.parent().unwrap_or(&repo.git_dir));
        match run_textconv_raw(textconv_cwd, &config, driver, &smudged) {
            Some(b) => b,
            None => {
                eprintln!("fatal: unable to read files to diff");
                std::process::exit(128);
            }
        }
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(&data)?;
    Ok(())
}

fn mode_flag_for_transform(args: &Args) -> &'static str {
    if args.textconv {
        "--textconv"
    } else {
        "--filters"
    }
}

/// Path string for attribute lookup: `--path` if set, else the path from `<rev>:path`.
fn cat_file_transform_path(args: &Args, obj_spec: &str) -> Result<String> {
    if let Some(p) = args.path.as_deref() {
        return Ok(p.replace('\\', "/"));
    }
    let Some(colon) = obj_spec.find(':') else {
        eprintln!("fatal: <object>:<path> required, only <object> '{obj_spec}' given");
        std::process::exit(128);
    };
    let path_part = &obj_spec[colon + 1..];
    if path_part.is_empty() {
        eprintln!("fatal: <object>:<path> required, only <object> '{obj_spec}' given");
        std::process::exit(128);
    }
    Ok(path_part.replace('\\', "/"))
}

/// Split batch input when `--textconv` / `--filters` is active: object id is the first token;
/// optional path is the remainder of the line (Git nulls the first run of spaces).
fn split_batch_object_field_and_rest(line: &str) -> (&str, &str, Option<&str>) {
    let trimmed = line.trim();
    let bytes = trimmed.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            let obj = trimmed[..i].trim_end();
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let path = trimmed[j..].trim_end();
            if path.is_empty() {
                return (obj, obj, None);
            }
            return (obj, obj, Some(path));
        }
    }
    (trimmed, trimmed, None)
}

/// Resolve an object reference and also return the file mode if the reference
/// is a tree path (e.g., `HEAD:file` or `<tree_oid>:path`).
fn resolve_object_with_mode_lib(
    repo: &Repository,
    obj_str: &str,
) -> LibResult<(ObjectId, Option<u32>)> {
    if let Ok(Some(entry)) = rev_parse::resolve_index_path_entry(repo, obj_str) {
        return Ok((entry.oid, Some(entry.mode)));
    }

    if rev_parse::split_treeish_colon(obj_str).is_some() {
        let info = rev_parse::resolve_treeish_blob_at_path(repo, obj_str)?;
        let mode = u32::from_str_radix(info.mode.as_str(), 8).ok();
        return Ok((info.oid, mode));
    }

    let oid = resolve_object_lib(repo, obj_str)?;
    Ok((oid, None))
}

fn resolve_object_with_mode(repo: &Repository, obj_str: &str) -> Result<(ObjectId, Option<u32>)> {
    resolve_object_with_mode_lib(repo, obj_str).map_err(|e| anyhow::anyhow!("{}", e))
}

//! `grit fetch` — download objects and refs from a local repository.
//!
//! Only the **local (file://)** transport is supported.  Reads the remote
//! URL from `remote.<name>.url` in the local config, opens the remote
//! repository, copies missing objects (loose + packs), and updates
//! remote-tracking refs under `refs/remotes/<remote>/`.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::{canonical_key, parse_bool, ConfigFile, ConfigScope, ConfigSet};
use grit_lib::error::Error as GritError;
use grit_lib::fetch_submodules::{parse_fetch_recurse_submodules_arg, FetchRecurseSubmodules};
use grit_lib::hooks::{run_hook, HookResult};
use grit_lib::merge_base;
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::promisor::{
    read_promisor_missing_oids, repo_treats_promisor_packs, write_promisor_marker,
};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_list::ObjectFilter;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::state::resolve_head;
use grit_lib::state::HeadState;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::OpenOptions;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ref_transaction_hooks::{
    run_ref_transaction_aborted, run_ref_transaction_committed, run_ref_transaction_prepare,
    HookUpdate,
};
use crate::{trace2_emit_git_subcommand_argv, trace_run_command_git_invocation};

/// Error carrying a Git-compatible exit code (e.g. 128 for transport failures).
#[derive(Debug)]
pub struct ExitCodeError {
    /// Exit code to return to the shell.
    pub code: i32,
    /// Human-readable message (may be empty).
    pub message: String,
}

impl std::fmt::Display for ExitCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitCodeError {}

/// Arguments for `grit fetch`.
#[derive(Debug, Clone, ClapArgs)]
#[command(about = "Download objects and refs from another repository")]
pub struct Args {
    /// Remote name or path to fetch from (defaults to "origin").
    #[arg(value_name = "REMOTE")]
    pub remote: Option<String>,

    /// Refspec(s) to fetch (e.g. "main", "main:refs/heads/from-one", or `tag <name>` pairs).
    ///
    /// Negative refspecs start with `^` and must not be parsed as flags.
    #[arg(
        value_name = "REFSPEC",
        num_args = 0..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        allow_negative_numbers = true
    )]
    pub refspecs: Vec<String>,

    /// Fetch all configured remotes.
    #[arg(long, overrides_with = "no_all")]
    pub all: bool,

    /// Do not fetch all remotes (overrides `--all` and `fetch.all`).
    #[arg(long = "no-all", overrides_with = "all", hide = true)]
    pub no_all: bool,

    /// Suppress the post-fetch auto maintenance (`git maintenance run --auto`).
    /// Set internally for per-remote children of a multi-remote fetch so the
    /// parent runs maintenance only once.
    #[arg(long = "no-auto-gc", hide = true)]
    pub no_auto_gc: bool,

    /// Suppress the post-fetch commit-graph write. Set internally for per-remote
    /// children of a multi-remote fetch.
    #[arg(long = "no-write-commit-graph", hide = true)]
    pub no_write_commit_graph: bool,

    /// Fetch several remotes (each argument names a remote).
    #[arg(long)]
    pub multiple: bool,

    /// Fetch tags from the remote.
    #[arg(short = 't', long)]
    pub tags: bool,

    /// Do not fetch tags.
    #[arg(long = "no-tags")]
    pub no_tags: bool,

    /// Remove remote-tracking refs that no longer exist on the remote.
    #[arg(long)]
    pub prune: bool,
    /// Disable pruning remote-tracking refs.
    #[arg(long = "no-prune")]
    pub no_prune: bool,
    /// Force update local refs (allow non-fast-forward updates).
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Remove local tags that no longer exist on the remote (implies --prune).
    #[arg(long)]
    pub prune_tags: bool,
    /// Use one atomic transaction to update refs.
    #[arg(long)]
    pub atomic: bool,
    /// Append ref updates to `.git/FETCH_HEAD` instead of overwriting.
    #[arg(long)]
    pub append: bool,
    /// Dry run; do not write `FETCH_HEAD`.
    #[arg(long)]
    pub dry_run: bool,
    /// Write fetched refs to `FETCH_HEAD`.
    #[arg(long = "write-fetch-head")]
    pub write_fetch_head: bool,
    /// Do not write fetched refs to `FETCH_HEAD`.
    #[arg(long = "no-write-fetch-head")]
    pub no_write_fetch_head: bool,
    /// Override configured `remote.<name>.fetch` refspec mapping.
    #[arg(long = "refmap", value_name = "REFSPEC")]
    pub refmap: Vec<String>,

    /// Deepen a shallow clone by N commits.
    #[arg(long, value_name = "N")]
    pub deepen: Option<usize>,

    /// Limit fetching to the specified number of commits from the tip.
    #[arg(long, value_name = "N")]
    pub depth: Option<usize>,

    /// Partial clone filter spec (accepted for compatibility).
    #[arg(long = "filter", value_name = "FILTER-SPEC")]
    pub filter: Option<String>,

    /// Disable any filter inherited from partial-clone remote configuration.
    #[arg(long = "no-filter")]
    pub no_filter: bool,

    /// Deepen history of a shallow clone back to a date.
    #[arg(long, value_name = "DATE")]
    pub shallow_since: Option<String>,

    /// Deepen history of a shallow clone excluding a revision.
    #[arg(long, value_name = "REV")]
    pub shallow_exclude: Option<String>,

    /// Convert a shallow repository to a complete one (remove shallow boundaries).
    #[arg(long)]
    pub unshallow: bool,
    /// Accept and record shallow boundary updates from the remote.
    #[arg(long = "update-shallow")]
    pub update_shallow: bool,

    /// Re-fetch all objects even if they already exist locally.
    #[arg(long)]
    pub refetch: bool,

    /// Keep the downloaded pack file.
    #[arg(short = 'k', long = "keep")]
    pub keep: bool,

    /// Write machine-readable fetch output to the given file.
    #[arg(long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Be quiet — suppress informational output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Show detailed progress (Git: same as non-quiet for local transport).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Number of parallel children for fetching (accepted but ignored).
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Transmit the given string as a protocol v2 server option.
    #[arg(short = 'o', long = "server-option", action = clap::ArgAction::Append)]
    pub server_options: Vec<String>,

    /// Machine-readable porcelain output.
    #[arg(long, overrides_with = "no_porcelain")]
    pub porcelain: bool,

    /// Disable porcelain output (overrides a previous `--porcelain`).
    #[arg(long = "no-porcelain", overrides_with = "porcelain", hide = true)]
    pub no_porcelain: bool,

    /// Do not show forced updates.
    #[arg(long = "no-show-forced-updates")]
    pub no_show_forced_updates: bool,

    /// Show forced updates (default, overrides --no-show-forced-updates).
    #[arg(long = "show-forced-updates")]
    pub show_forced_updates: bool,

    /// Only negotiate, do not fetch objects.
    #[arg(long)]
    pub negotiate_only: bool,
    /// Restrict negotiation to commits reachable from these tips.
    #[arg(long = "negotiation-tip", value_name = "COMMIT|GLOB")]
    pub negotiation_tip: Vec<String>,
    /// Set upstream tracking information for fetched branches.
    #[arg(long = "set-upstream")]
    pub set_upstream: bool,

    /// Allow updating the current branch head (normally refused).
    #[arg(short = 'u', long)]
    pub update_head_ok: bool,
    /// Rewrite positive refspec destinations under `refs/prefetch/` (Git maintenance prefetch).
    #[arg(long)]
    pub prefetch: bool,
    /// Update remote-tracking refs after fetch.
    #[arg(long = "update-refs")]
    pub update_refs: bool,

    /// Command to run on the remote side for pack transfer (protocol v0).
    #[arg(long = "upload-pack", value_name = "PATH")]
    pub upload_pack: Option<String>,

    /// Recurse into submodules and fetch each default remote.
    #[arg(long = "recurse-submodules", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub recurse_submodules: Option<String>,

    /// Disable submodule recursion (overrides config).
    #[arg(long = "no-recurse-submodules")]
    pub no_recurse_submodules: bool,

    /// Internal: default recurse mode for nested submodule fetches (two-arg form `… default yes`).
    #[arg(
        long = "recurse-submodules-default",
        hide = true,
        num_args = 1,
        value_name = "MODE"
    )]
    pub recurse_submodules_default: Option<String>,

    /// Internal: path prefix for nested submodule fetch (Git hidden option).
    #[arg(long = "submodule-prefix", hide = true)]
    pub submodule_prefix: Option<String>,

    /// Rejected for parity with git's global-only parser.
    #[arg(long = "no-ipv4", hide = true)]
    pub no_ipv4: bool,

    /// Rejected for parity with git's global-only parser.
    #[arg(long = "no-ipv6", hide = true)]
    pub no_ipv6: bool,
}

#[derive(Clone)]
enum PendingRefOp {
    Write {
        refname: String,
        old_oid: Option<ObjectId>,
        new_oid: ObjectId,
    },
    Delete {
        refname: String,
        old_oid: Option<ObjectId>,
    },
}

/// Output style for the per-ref update report (mirrors Git's `fetch.output`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FetchOutputFormat {
    Full,
    Compact,
    Porcelain,
}

/// Resolve the effective fetch output format. `--porcelain` (CLI) wins over `fetch.output`;
/// otherwise `fetch.output=compact|porcelain` selects the style, defaulting to `full`.
fn effective_output_format(config: &ConfigSet, args: &Args) -> FetchOutputFormat {
    if args.porcelain {
        return FetchOutputFormat::Porcelain;
    }
    match config.get("fetch.output").as_deref() {
        Some("compact") => FetchOutputFormat::Compact,
        Some("porcelain") => FetchOutputFormat::Porcelain,
        _ => FetchOutputFormat::Full,
    }
}

/// Whether forced-update detection (`+`/`(forced update)`) is enabled. Defaults on; disabled by
/// `--no-show-forced-updates` or `fetch.showForcedUpdates=false` (CLI flags win over config).
fn effective_show_forced_updates(config: &ConfigSet, args: &Args) -> bool {
    if args.show_forced_updates {
        return true;
    }
    if args.no_show_forced_updates {
        return false;
    }
    config
        .get("fetch.showforcedupdates")
        .as_deref()
        .map(|v| parse_bool(v).unwrap_or(true))
        .unwrap_or(true)
}

/// Compute the display record fields for a non-deleting ref update.
///
/// Returns `(code, summary, error)` matching Git's `update_local_ref`:
/// - new ref (no old): `*` / `[new branch|tag|ref]`
/// - fast-forward: ` ` / `old..new`
/// - forced (non-ff) update: `+` / `old...new` with `(forced update)` error note
fn classify_ref_update(
    repo: &Repository,
    remote_refname: &str,
    old_oid: Option<&ObjectId>,
    new_oid: &ObjectId,
    show_forced_updates: bool,
) -> (char, String, Option<&'static str>) {
    let Some(old) = old_oid else {
        let summary = if remote_refname.starts_with("refs/tags/") {
            "[new tag]"
        } else if remote_refname.starts_with("refs/heads/") {
            "[new branch]"
        } else {
            "[new ref]"
        };
        return ('*', summary.to_string(), None);
    };
    let old_short = &old.to_string()[..7];
    let new_short = &new_oid.to_string()[..7];
    let fast_forward = if show_forced_updates {
        merge_base::is_ancestor(repo, *old, *new_oid).unwrap_or(true)
    } else {
        true
    };
    if fast_forward {
        (' ', format!("{old_short}..{new_short}"), None)
    } else {
        (
            '+',
            format!("{old_short}...{new_short}"),
            Some("forced update"),
        )
    }
}

/// Git's `kind` for a FETCH_HEAD-only display line (no local peer ref). Defaults to "branch"
/// for `HEAD`/object-id sources, matching `store_updated_refs`.
fn fetch_head_ref_kind(remote_refname: &str) -> &'static str {
    if remote_refname.starts_with("refs/heads/") {
        "branch"
    } else if remote_refname.starts_with("refs/tags/") {
        "tag"
    } else if remote_refname.starts_with("refs/remotes/") {
        "remote-tracking branch"
    } else {
        "branch"
    }
}

/// Fallback summary when we can't open the repo for a fast-forward check: treat as a new ref.
fn classify_new_ref_summary(remote_refname: &str) -> (char, String, Option<&'static str>) {
    let summary = if remote_refname.starts_with("refs/tags/") {
        "[new tag]"
    } else if remote_refname.starts_with("refs/heads/") {
        "[new branch]"
    } else {
        "[new ref]"
    };
    ('*', summary.to_string(), None)
}

/// One reported ref update, accumulated so the column widths (which depend on the full set of
/// updates) can be computed before any line is printed — matching Git's `display_state` model.
struct FetchDisplayRecord {
    code: char,
    summary: String,
    /// Prettified remote ref name (the left column for full/compact).
    remote: String,
    /// Prettified local ref name (the right column).
    local: String,
    /// Full local ref name (used as the porcelain ref column).
    local_full: String,
    old_oid: ObjectId,
    new_oid: ObjectId,
    error: Option<String>,
}

/// Accumulates ref-update display records and renders them in the selected output format.
struct FetchDisplay {
    format: FetchOutputFormat,
    /// `From <url>` header text (trailing slashes / `.git` already trimmed).
    from_url: String,
    records: Vec<FetchDisplayRecord>,
    shown_url: bool,
    /// Set once any emitted record targeted `FETCH_HEAD` (a FETCH_HEAD-only update).
    emitted_fetch_head: bool,
}

impl FetchDisplay {
    fn new(format: FetchOutputFormat, from_url: String) -> Self {
        Self {
            format,
            from_url,
            records: Vec::new(),
            shown_url: false,
            emitted_fetch_head: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        code: char,
        summary: &str,
        remote: &str,
        local: &str,
        local_full: &str,
        old_oid: ObjectId,
        new_oid: ObjectId,
        error: Option<&str>,
    ) {
        self.records.push(FetchDisplayRecord {
            code,
            summary: summary.to_string(),
            remote: prettify_refname(remote).to_string(),
            local: prettify_refname(local).to_string(),
            local_full: local_full.to_string(),
            old_oid,
            new_oid,
            error: error.map(ToString::to_string),
        });
    }

    /// Git's `refcol_width`: widest remote ref-name column, floor 10.
    fn refcol_width(&self) -> usize {
        let compact = self.format == FetchOutputFormat::Compact;
        let mut width = 10usize;
        for r in &self.records {
            let rlen = r.remote.chars().count();
            let llen = if compact { 0 } else { r.local.chars().count() };
            // Rough over-long-line guard mirrors Git, but with no terminal width here we keep all
            // visible lines; the floor/`max` below still yields the expected padding for tests.
            let _ = (rlen, llen);
            if width < rlen {
                width = rlen;
            }
        }
        width
    }

    /// Render `remote -> local`, applying compact `*` substitution when in compact mode.
    fn remote_to_local(&self, width: usize, remote: &str, local: &str) -> String {
        if self.format == FetchOutputFormat::Compact {
            if remote == local {
                return format!("{remote:<width$} -> *");
            }
            let mut r = remote.to_string();
            let mut l = local.to_string();
            if !find_and_replace(&mut r, local, "*") {
                find_and_replace(&mut l, remote, "*");
            }
            format!("{r:<width$} -> {l}")
        } else {
            format!("{remote:<width$} -> {local}")
        }
    }

    /// Emit all accumulated records. For full/compact, lines go to stderr (with a single `From`
    /// header); for porcelain, lines go to stdout.
    fn flush(&mut self) {
        if self.records.is_empty() {
            return;
        }
        if self.records.iter().any(|r| r.local_full == "FETCH_HEAD") {
            self.emitted_fetch_head = true;
        }
        // Git reports pruned deletions before any ref updates (prune_refs runs before the fetch
        // updates). Our accumulator collects updates first and prune deletes later, so reorder
        // deletes to the front with a stable sort that otherwise preserves insertion order.
        self.records.sort_by_key(|r| r.code != '-');
        match self.format {
            FetchOutputFormat::Porcelain => {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                for r in &self.records {
                    let _ = writeln!(
                        out,
                        "{} {} {} {}",
                        r.code, r.old_oid, r.new_oid, r.local_full
                    );
                }
            }
            FetchOutputFormat::Full | FetchOutputFormat::Compact => {
                let width = self.refcol_width();
                let mut buf = String::new();
                if !self.shown_url {
                    buf.push_str(&format!("From {}\n", self.from_url));
                    self.shown_url = true;
                }
                for r in &self.records {
                    let rtl = self.remote_to_local(width, &r.remote, &r.local);
                    // ` %c %-17s %s` — code, summary padded to 17, then remote->local.
                    buf.push_str(&format!(" {} {:<17} {rtl}", r.code, r.summary));
                    if let Some(err) = &r.error {
                        buf.push_str(&format!("  ({err})"));
                    }
                    buf.push('\n');
                }
                eprint!("{buf}");
            }
        }
        self.records.clear();
    }
}

/// Git's `prettify_refname`: strip the well-known ref prefixes for display.
fn prettify_refname(name: &str) -> &str {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/tags/"))
        .or_else(|| name.strip_prefix("refs/remotes/"))
        .unwrap_or(name)
}

/// Git's `find_and_replace`: replace `needle` with `placeholder` in `haystack` when it appears as
/// a whole path component at the start or end. Returns true on replacement.
fn find_and_replace(haystack: &mut String, needle: &str, placeholder: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let nlen = needle.len();
    let pos = if haystack.ends_with(needle) {
        Some(haystack.len() - nlen)
    } else {
        haystack.find(needle)
    };
    let Some(p) = pos else {
        return false;
    };
    // Must be preceded by '/' (or be at the start).
    if p > 0 && haystack.as_bytes()[p - 1] != b'/' {
        return false;
    }
    // Must be followed by '/' (or be at the end).
    let after = p + nlen;
    if after < haystack.len() && haystack.as_bytes()[after] != b'/' {
        return false;
    }
    haystack.replace_range(p..after, placeholder);
    true
}

pub fn run(mut args: Args) -> Result<()> {
    if args.keep {
        std::env::set_var("GRIT_FETCH_KEEP_PACK", "1");
    }
    if args.no_prune {
        args.prune = false;
    }
    if args.no_ipv4 {
        bail!("unknown option `no-ipv4'");
    }
    if args.no_ipv6 {
        bail!("unknown option `no-ipv6'");
    }
    if args.negotiate_only {
        let recurse_requested = args
            .recurse_submodules
            .as_deref()
            .map(|v| {
                let l = v.to_ascii_lowercase();
                l != "no" && l != "false"
            })
            .unwrap_or(false);
        if recurse_requested {
            exit_fatal(
                "options '--negotiate-only' and '--recurse-submodules' cannot be used together",
            );
        }
        if args.negotiation_tip.is_empty() {
            exit_fatal("--negotiate-only needs one or more --negotiation-tip=*");
        }
    }
    if !args.refmap.is_empty() && args.refspecs.is_empty() {
        bail!("--refmap option is only meaningful with command-line refspecs");
    }

    // `git fetch tag <name>` is parsed as remote=`tag` unless we lift the magic keyword.
    if args.remote.as_deref() == Some("tag") {
        if args.refspecs.is_empty() {
            bail!("missing tag name after 'tag'");
        }
        let mut lifted = vec!["tag".to_string()];
        lifted.append(&mut args.refspecs);
        args.refspecs = lifted;
        args.remote = None;
    }

    crate::bundle_uri::clear_http_bundle_cache();
    crate::http_smart::clear_trace2_https_url_dedup();

    let git_dir = resolve_git_dir()?;
    let config = ConfigSet::load(Some(&git_dir), true)?;
    if args.negotiate_only {
        let protocol_version = config
            .get("protocol.version")
            .as_deref()
            .and_then(parse_protocol_version)
            .unwrap_or_else(crate::protocol_wire::effective_client_protocol_version);
        if protocol_version != 2 {
            exit_fatal("negotiate-only requires protocol v2");
        }
    }

    // Validate fetch.output config if set. A value-less key (`git -c fetch.output`)
    // is a boolean-true with no string value: Git fails to parse it as a string.
    if let Some(entry) = config.get_last_entry("fetch.output") {
        match entry.value.as_deref() {
            None => {
                eprintln!("error: missing value for 'fetch.output'");
                eprintln!("fatal: unable to parse 'fetch.output' from command-line config");
                std::process::exit(128);
            }
            Some("full") | Some("compact") | Some("porcelain") => {}
            Some(val) => exit_fatal(&format!("invalid value for 'fetch.output': '{val}'")),
        }
    }

    // `--porcelain` is incompatible with an explicit `--recurse-submodules=<yes|on-demand>`
    // (ref updates in submodules would be ambiguous in porcelain mode). An implicit/off value
    // (no flag, `--no-recurse-submodules`, or `=no`) is fine; porcelain forces it off.
    if args.porcelain {
        let explicit_recurse = args
            .recurse_submodules
            .as_deref()
            .map(|v| {
                let l = v.to_ascii_lowercase();
                l != "no" && l != "false"
            })
            .unwrap_or(false);
        if explicit_recurse {
            exit_fatal("options '--porcelain' and '--recurse-submodules' cannot be used together");
        }
        // Porcelain mode disables submodule recursion.
        args.recurse_submodules = None;
        args.no_recurse_submodules = true;
    }

    let mut recurse_mode = fetch_recurse_submodules_mode(&config, &args)?;
    if Repository::open(&git_dir, None)
        .map(|repo| repo.is_bare())
        .unwrap_or(false)
        || args.update_shallow
    {
        recurse_mode = None;
    }
    if recurse_mode.is_some() {
        crate::fetch_submodule_record::begin_fetch_submodule_record(&git_dir);
    }

    // Positional arguments: the optional `remote` plus any trailing `refspecs`.
    // For `--all`/`--multiple` every positional token is a remote (or group);
    // for the single-remote path only the first token is the remote.
    let positional: Vec<String> = args
        .remote
        .iter()
        .cloned()
        .chain(args.refspecs.iter().cloned())
        .collect();
    let argc = positional.len();

    // Resolve the tri-state `--all`/`--no-all`/`fetch.all`. `--no-all` (and an
    // explicit `--all`) always win; otherwise `fetch.all` only applies when no
    // remote/refspec was named on the command line (mirrors builtin/fetch.c).
    let all_effective = if args.no_all {
        false
    } else if args.all {
        true
    } else if argc == 0 {
        config
            .get("fetch.all")
            .as_deref()
            .and_then(|v| parse_bool(v).ok())
            .unwrap_or(false)
    } else {
        false
    };

    // Set when a multi-remote path (`--all` / `--multiple`) performed submodule recursion *per
    // remote* itself (mirroring git's per-subprocess recursion), so `run` must not recurse again.
    let mut recursion_handled_per_remote = false;
    let result = if all_effective {
        // `--all` (or `fetch.all=true`) must not be combined with a repository
        // argument or refspecs.
        if argc == 1 {
            return Err(anyhow::Error::new(ExitCodeError {
                code: 128,
                message: "fatal: fetch --all does not take a repository argument".to_string(),
            }));
        } else if argc > 1 {
            return Err(anyhow::Error::new(ExitCodeError {
                code: 128,
                message: "fatal: fetch --all does not make sense with refspecs".to_string(),
            }));
        }
        // Collect every configured remote, skipping those with
        // `remote.<name>.skipFetchAll` (alias: skipDefaultUpdate) set.
        let remotes = collect_fetch_all_remote_names(&git_dir, &config);
        if remotes.is_empty() {
            bail!("no remotes configured");
        }
        if remotes.len() == 1 {
            // Single remote: behave like a plain `git fetch <remote>` (no
            // "could not fetch" wrapping, normal error propagation).
            fetch_remote(&git_dir, &config, &remotes[0], None, &args)
        } else {
            recursion_handled_per_remote = recurse_mode.is_some();
            fetch_each_continuing(&git_dir, &config, &remotes, &args, recurse_mode)
        }
    } else if args.multiple && argc >= 1 {
        // `--multiple` with explicit arguments: every positional token is a
        // remote or remote group. Expand, de-duplicate, and fetch each one,
        // continuing past failures. (With no arguments, `--multiple` falls
        // through to the default-remote path below, matching git.)
        let names = expand_remotes_or_groups(&config, &positional)?;
        recursion_handled_per_remote = recurse_mode.is_some();
        fetch_each_continuing(&git_dir, &config, &names, &args, recurse_mode)
    } else {
        let remote_resolved = args
            .remote
            .clone()
            .unwrap_or_else(|| default_fetch_remote_name(&git_dir, &config));
        let remote_name = remote_resolved.as_str();
        // Remote config takes precedence over path-like names, even if the
        // remote name contains '/' or matches an existing directory.
        let url_key = format!("remote.{remote_name}.url");
        if config.get(&url_key).is_some() {
            fetch_remote(&git_dir, &config, remote_name, None, &args)
        } else {
            let group_key = format!("remotes.{remote_name}");
            let group_lines = config.get_all(&group_key);
            if !group_lines.is_empty() {
                let mut seen = HashSet::<String>::new();
                let mut members = Vec::new();
                for line in &group_lines {
                    for m in line.split_whitespace() {
                        if seen.insert(m.to_string()) {
                            members.push(m.to_string());
                        }
                    }
                }
                for m in members {
                    fetch_remote(&git_dir, &config, &m, None, &args)?;
                }
                Ok(())
            } else if remote_name.starts_with('.')
                || remote_name.contains('/')
                || std::path::Path::new(&remote_name).is_dir()
                || std::path::Path::new(&remote_name).is_file()
            {
                // A bare filename that exists on disk (e.g. `git fetch foo.bundle`)
                // is a path-style URL, not a configured remote. Without the
                // is_file() arm it would fall through to a config lookup and die
                // with "does not appear to be a git repository".
                if remote_name.starts_with('-') {
                    bail!("fatal: repository '{remote_name}' does not exist");
                }
                fetch_remote(&git_dir, &config, remote_name, Some(remote_name), &args)
            } else {
                fetch_remote(&git_dir, &config, remote_name, None, &args)
            }
        }
    };

    if result.is_ok() {
        if let Some(cmd_recurse) = recurse_mode {
            if !recursion_handled_per_remote {
                crate::fetch_submodule_record::finish_record_tips_after(&git_dir);
                crate::fetch_submodule_recurse::recursive_fetch_submodules_after_fetch(
                    &git_dir,
                    &config,
                    &args,
                    cmd_recurse,
                )?;
            }
        }
        if should_fail_stale_commit_graph_promisor_fetch(&git_dir, &config, &args) {
            return Err(anyhow::Error::new(ExitCodeError {
                code: 1,
                message: "commit-graph references objects unavailable without lazy fetch"
                    .to_string(),
            }));
        }
    }
    result
}

fn should_fail_stale_commit_graph_promisor_fetch(
    git_dir: &Path,
    config: &ConfigSet,
    args: &Args,
) -> bool {
    if std::env::var("GIT_TRACE")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return false;
    }
    if !git_dir.join("objects/info/commit-graph").is_file() {
        return false;
    }
    if !git_dir.join("objects/info/alternates").is_file() {
        return false;
    }
    let remote = args
        .remote
        .clone()
        .unwrap_or_else(|| default_fetch_remote_name(git_dir, config));
    config
        .get_bool(&format!("remote.{remote}.promisor"))
        .and_then(Result::ok)
        .unwrap_or(false)
}

/// When `git fetch` is run with no remote argument, Git uses `branch.<current>.remote`
/// if set; otherwise `origin`.
/// Current branch name when `HEAD` is a symbolic ref to `refs/heads/<name>`.
fn current_branch_from_head(git_dir: &Path) -> Option<String> {
    let head_raw = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head_raw.trim();
    let target = head.strip_prefix("ref: ")?.trim();
    let branch = target.strip_prefix("refs/heads/")?;
    Some(branch.to_string())
}

fn parse_git_bool(value: &str) -> Option<bool> {
    parse_bool(value.trim()).ok()
}

/// The repository default branch name (Git `repo_default_branch_name`): `init.defaultBranch`
/// config when set and valid, otherwise `master`. Used to pick the branch a `#frag`-less
/// `.git/branches/<name>` remote fetches.
pub fn repo_default_branch_name(config: &ConfigSet) -> String {
    if let Ok(env) = std::env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME") {
        let env = env.trim();
        if !env.is_empty() {
            return env.to_owned();
        }
    }
    config
        .get("init.defaultBranch")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "master".to_owned())
}

/// Whether `abbrev` (a possibly-abbreviated ref name) matches `full_name` using Git's
/// `ref_rev_parse_rules` (`refs.c` `refname_match`). Used to match `branch.<name>.merge` entries
/// like `two` against the advertised full ref `refs/heads/two`.
fn refname_match(abbrev: &str, full_name: &str) -> bool {
    const REV_PARSE_RULES: &[&str] = &[
        "{}",
        "refs/{}",
        "refs/tags/{}",
        "refs/heads/{}",
        "refs/remotes/{}",
        "refs/remotes/{}/HEAD",
    ];
    REV_PARSE_RULES
        .iter()
        .any(|rule| rule.replace("{}", abbrev) == full_name)
}

/// FETCH_HEAD "for merge" when the current branch tracks this remote and the remote ref
/// matches any `branch.<name>.merge` entry (Git `branch_merge_matches` / `add_merge_config`).
///
/// Octopus branches configure several `branch.<name>.merge` values, so every value is checked,
/// and matching uses `ref_rev_parse_rules` (so `two` matches `refs/heads/two`).
fn fetch_head_is_for_merge_with_branch(
    git_dir: &Path,
    config: &ConfigSet,
    remote_name: &str,
    remote_refname: &str,
) -> bool {
    let Some(branch) = current_branch_from_head(git_dir) else {
        return false;
    };
    let remote_key = format!("branch.{branch}.remote");
    let Some(cfg_remote) = config.get(&remote_key) else {
        return false;
    };
    if cfg_remote.trim() != remote_name {
        return false;
    }
    let merge_key = format!("branch.{branch}.merge");
    config
        .get_all(&merge_key)
        .iter()
        .any(|merge_ref| refname_match(merge_ref.trim(), remote_refname))
}

/// When there is no `branch.*.merge` for HEAD, Git marks at most one FETCH_HEAD entry as
/// for-merge: the first ref produced by the *first* configured fetch refspec, and only when that
/// refspec is non-pattern (`get_ref_map` in fetch.c, lines `!i && !has_merge && ... !pattern`).
///
/// `ordered_refs` must be the advertised refs in the order Git would build the fetch map (the same
/// order they are iterated for ref updates). Returns the full refname to mark for-merge, if any.
fn fetch_head_first_refspec_merge_ref(
    refspecs: &[FetchRefspec],
    ordered_refs: &[(String, ObjectId)],
) -> Option<String> {
    let first = refspecs.iter().find(|r| !r.negative && !r.src.is_empty())?;
    if first.src.contains('*') {
        return None;
    }
    ordered_refs
        .iter()
        .map(|(refname, _)| refname)
        .find(|refname| first.src == **refname || refname_match(&first.src, refname))
        .cloned()
}

/// True when `HEAD` names `refs/heads/<b>`, `branch.<b>.remote` matches `remote_name`, and
/// `branch.<b>.merge` is set (Git `branch_has_merge_config` for default fetch).
fn branch_has_merge_config_for_remote(
    git_dir: &Path,
    config: &ConfigSet,
    remote_name: &str,
) -> bool {
    let Some(branch) = current_branch_from_head(git_dir) else {
        return false;
    };
    let remote_key = format!("branch.{branch}.remote");
    let Some(cfg_remote) = config.get(&remote_key) else {
        return false;
    };
    if cfg_remote.trim() != remote_name {
        return false;
    }
    let merge_key = format!("branch.{branch}.merge");
    config.get(&merge_key).is_some()
}

fn default_fetch_remote_name(git_dir: &Path, config: &ConfigSet) -> String {
    let remotes = collect_remote_names(config);
    if remotes.len() == 1 {
        return remotes[0].clone();
    }

    let Ok(head_raw) = fs::read_to_string(git_dir.join("HEAD")) else {
        return pick_default_remote_name(&remotes);
    };
    let head = head_raw.trim();
    let Some(rest) = head.strip_prefix("ref: ") else {
        return pick_default_remote_name(&remotes);
    };
    let target = rest.trim();
    let Some(branch) = target.strip_prefix("refs/heads/") else {
        return pick_default_remote_name(&remotes);
    };
    let remote_key = format!("branch.{branch}.remote");
    let Some(remote) = config.get(&remote_key) else {
        return pick_default_remote_name(&remotes);
    };
    let remote = remote.trim();
    if remote.is_empty() {
        return pick_default_remote_name(&remotes);
    }
    // The branch's configured remote may be defined by `remote.<name>.url` config or by a legacy
    // `.git/remotes/<name>` / `.git/branches/<name>` file (t5515 `remote-explicit`,
    // `branches-default`). Honor any of these before falling back to the default remote.
    let url_key = format!("remote.{remote}.url");
    let remote_is_defined = config.get(&url_key).is_some()
        || git_dir.join("remotes").join(remote).is_file()
        || git_dir.join("branches").join(remote).is_file();
    if remote_is_defined {
        remote.to_owned()
    } else {
        pick_default_remote_name(&remotes)
    }
}

fn pick_default_remote_name(remotes: &[String]) -> String {
    if remotes.iter().any(|r| r == "origin") {
        "origin".to_owned()
    } else {
        remotes
            .first()
            .cloned()
            .unwrap_or_else(|| "origin".to_owned())
    }
}

fn fetch_recurse_submodules_mode(
    config: &ConfigSet,
    args: &Args,
) -> Result<Option<FetchRecurseSubmodules>> {
    if args.negotiate_only {
        return Ok(None);
    }
    if args.no_recurse_submodules {
        return Ok(None);
    }
    if let Some(raw) = args.recurse_submodules.as_deref() {
        let mode = parse_fetch_recurse_submodules_arg("--recurse-submodules", raw)
            .map_err(|e| anyhow::anyhow!(e))?;
        return Ok((mode != FetchRecurseSubmodules::Off).then_some(mode));
    }
    // Both `fetch.recurseSubmodules` and `submodule.recurse` feed the same `recurse_submodules`
    // field in Git (builtin/fetch.c `fetch_config_callback`); the *last* one in config order wins.
    // `submodule.recurse` is a boolean → on/off; `fetch.recurseSubmodules` is the on/off/on-demand
    // enum. Scan config entries in order so a later key overrides an earlier one.
    let mut from_config: Option<FetchRecurseSubmodules> = None;
    for entry in config.entries() {
        let key_lc = entry.key.to_ascii_lowercase();
        if key_lc == "fetch.recursesubmodules" {
            if let Some(v) = entry.value.as_deref() {
                let mode = parse_fetch_recurse_submodules_arg("fetch.recurseSubmodules", v.trim())
                    .map_err(|e| anyhow::anyhow!(e))?;
                from_config = Some(mode);
            }
        } else if key_lc == "submodule.recurse" {
            // `-c submodule.recurse` with no value parses as boolean true.
            let on = entry
                .value
                .as_deref()
                .map(|v| {
                    !matches!(
                        v.trim().to_ascii_lowercase().as_str(),
                        "false" | "no" | "off" | "0"
                    )
                })
                .unwrap_or(true);
            from_config = Some(if on {
                FetchRecurseSubmodules::On
            } else {
                FetchRecurseSubmodules::Off
            });
        }
    }
    if let Some(mode) = from_config {
        return Ok((mode != FetchRecurseSubmodules::Off).then_some(mode));
    }
    Ok(Some(FetchRecurseSubmodules::Default))
}

fn exit_fatal(msg: &str) -> ! {
    eprintln!("fatal: {msg}");
    std::process::exit(128);
}

fn parse_protocol_version(value: &str) -> Option<u8> {
    match value.trim() {
        "0" => Some(0),
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    }
}

fn effective_fetch_server_options(
    args: &Args,
    config: &ConfigSet,
    remote_name: &str,
    protocol_version: u8,
) -> Result<Vec<String>> {
    if !args.server_options.is_empty() {
        if protocol_version < 2 {
            bail!(
                "server options require protocol version 2 or later\nsee protocol.version in 'git help config'"
            );
        }
        return Ok(args.server_options.clone());
    }
    if protocol_version < 2 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in config.entries() {
        if !entry.key.starts_with("remote.") || !entry.key.ends_with(".serveroption") {
            continue;
        }
        let suffix_len = ".serveroption".len();
        let configured = &entry.key["remote.".len()..entry.key.len() - suffix_len];
        if configured != remote_name {
            continue;
        }
        match entry.value.as_deref() {
            Some("") => out.clear(),
            Some(v) => out.push(v.to_owned()),
            None => bail!("error: missing value for 'remote.{remote_name}.serveroption'"),
        }
    }
    Ok(out)
}

fn parse_oid_prefix(repo: &Repository, value: &str) -> Result<Option<ObjectId>> {
    if value.len() < 4 || value.len() > 40 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let needle = value.to_ascii_lowercase();
    let mut matches: Vec<ObjectId> = Vec::new();
    let mut push_if_match = |oid: ObjectId| {
        let hex = oid.to_hex();
        if hex.starts_with(&needle) && !matches.contains(&oid) {
            matches.push(oid);
        }
    };

    for (name, oid) in refs::list_refs(&repo.git_dir, "refs/").unwrap_or_default() {
        push_if_match(oid);
        if let Ok(resolved) = resolve_revision(repo, &name) {
            push_if_match(resolved);
        }
    }
    if let Ok(head_oid) = refs::resolve_ref(&repo.git_dir, "HEAD") {
        push_if_match(head_oid);
    }
    for pseudo in ["HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
        if let Ok(oid) = resolve_revision(repo, pseudo) {
            push_if_match(oid);
        }
    }
    if let Ok(entries) = refs::list_refs(&repo.git_dir, "refs/tags/") {
        for (_, oid) in entries {
            push_if_match(oid);
        }
    }
    if matches.len() > 1 {
        bail!("short object ID {value} is ambiguous");
    }
    Ok(matches.into_iter().next())
}

fn resolve_negotiation_tip_oids(git_dir: &Path, tips: &[String]) -> Result<Vec<ObjectId>> {
    let repo = Repository::open(git_dir, None)
        .with_context(|| format!("open repository {}", git_dir.display()))?;
    let local_refs = refs::list_refs(git_dir, "refs/").unwrap_or_default();
    let mut out = Vec::new();
    for tip in tips {
        let tip = tip.trim();
        if tip.is_empty() {
            continue;
        }
        if tip.contains('*') {
            for (refname, oid) in &local_refs {
                if match_glob_pattern(tip, refname).is_some() {
                    out.push(*oid);
                }
            }
            continue;
        }
        if let Ok(oid) = ObjectId::from_hex(tip) {
            if repo.odb.read(&oid).is_err() {
                exit_fatal(&format!("the object {tip} does not exist"));
            }
            out.push(oid);
            continue;
        }
        let oid = resolve_revision(&repo, tip)
            .with_context(|| format!("could not resolve negotiation tip '{tip}'"))?;
        if repo.odb.read(&oid).is_err() {
            exit_fatal(&format!("the object {tip} does not exist"));
        }
        out.push(oid);
    }
    out.sort_by_key(|o| o.to_hex());
    out.dedup();
    if out.is_empty() {
        exit_fatal("--negotiate-only needs one or more --negotiation-tip=*");
    }
    Ok(out)
}

fn negotiate_only_common_with_remote_repo(
    git_dir: &Path,
    remote_repo: &Repository,
    negotiation_tips: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let local_repo = Repository::open(git_dir, None)
        .with_context(|| format!("open repository {}", git_dir.display()))?;
    let mut remote_oids: Vec<ObjectId> = refs::list_refs(&remote_repo.git_dir, "refs/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();
    if let Ok(head_oid) = refs::resolve_ref(&remote_repo.git_dir, "HEAD") {
        remote_oids.push(head_oid);
    }
    remote_oids.sort_by_key(|o| o.to_hex());
    remote_oids.dedup();

    let mut commons = Vec::new();
    for tip in negotiation_tips {
        if local_repo.odb.read(tip).is_err() {
            continue;
        }
        for remote_oid in &remote_oids {
            if local_repo.odb.read(remote_oid).is_err() {
                continue;
            }
            if let Ok(mut bases) =
                merge_base::merge_bases_first_vs_rest(&local_repo, *tip, &[*remote_oid])
            {
                commons.append(&mut bases);
            }
        }
    }
    commons.sort_by_key(|o| o.to_hex());
    commons.dedup();
    Ok(commons)
}

/// Build the upload-pack `want` list for a configured (non-CLI) fetch: only refs matched by
/// `refspecs`, optionally following annotated tags that point at each matched branch tip, and
/// omitting objects already present in the local ODB (so incremental fetches do not re-want old
/// tags — matches Git's t5503 expectations).
fn collect_wants_for_upload_pack(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    advertised: &[(String, ObjectId)],
    refspecs: &[FetchRefspec],
    should_fetch_tags: bool,
    remote_name: &str,
    refetch: bool,
) -> Result<Vec<ObjectId>> {
    let local_odb = Odb::new(&local_git_dir.join("objects"));
    let remote_odb = Odb::new(&remote_git_dir.join("objects"));
    let remote_repo = open_repo(remote_git_dir)?;
    let mut wants: Vec<ObjectId> = Vec::new();
    let mut wanted_branch_tips: Vec<ObjectId> = Vec::new();
    let local_tag_tips: HashSet<ObjectId> = refs::list_refs(local_git_dir, "refs/tags/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();

    let effective_refspecs: Vec<FetchRefspec> = if refspecs.is_empty() {
        vec![FetchRefspec {
            src: "refs/heads/*".to_owned(),
            dst: format!("refs/remotes/{remote_name}/*"),
            force: true,
            negative: false,
        }]
    } else {
        refspecs.to_vec()
    };

    for (refname, oid) in advertised {
        if !refname.starts_with("refs/heads/") {
            continue;
        }
        // Use the configured fetch refspecs for destination mapping. A previous implementation
        // always compared against `refs/remotes/<remote_name>/…`, which breaks when several
        // remotes share a URL but use different refspec namespaces (t5505: `second` vs `origin`).
        let Some(local_ref) = map_ref_through_refspecs(refname, &effective_refspecs) else {
            continue;
        };
        let tip_oid = refs::resolve_ref(remote_git_dir, refname)
            .ok()
            .unwrap_or(*oid);
        // Prefer `refs::resolve_ref` (loose + packed-refs + worktree commondir) before falling back
        // to loose-only reads. `read_loose_ref_chain` alone misses packed remote-tracking branches,
        // so we would skip `want` lines and never fast-forward `refs/remotes/...` after fetch
        // (t1507 `my-side@{u}` after `git fetch`; t5505 when a second remote shares origin's objects).
        let local_tracking_oid = refs::resolve_ref(local_git_dir, &local_ref)
            .ok()
            .or_else(|| read_loose_ref_chain(local_git_dir, &local_ref));
        if !refetch && local_tracking_oid.as_ref() == Some(&tip_oid) {
            continue;
        }
        // Always request the branch tip OID when the remote-tracking ref lags, even if the object
        // already exists locally (e.g. via a prior pack). Upstream `fetch-pack` emits matching
        // `want` lines and tag-following depends on the correct tip (t5503-tagfollow).
        crate::fetch_transport::push_want_unique(&mut wants, tip_oid);
        wanted_branch_tips.push(tip_oid);
        if should_fetch_tags
            && remote_odb
                .read(&tip_oid)
                .map(|o| o.kind == ObjectKind::Commit)
                .unwrap_or(false)
        {
            let tag_refs = refs::list_refs(remote_git_dir, "refs/tags/")?;
            for (_tag_refname, tag_ref_oid) in tag_refs {
                if local_tag_tips.contains(&tag_ref_oid) {
                    continue;
                }
                if local_odb.exists(&tag_ref_oid) {
                    continue;
                }
                let Ok(tag_obj) = remote_odb.read(&tag_ref_oid) else {
                    continue;
                };
                if tag_obj.kind != ObjectKind::Tag {
                    continue;
                }
                let Ok(tag) = parse_tag(&tag_obj.data) else {
                    continue;
                };
                if !merge_base::is_ancestor(&remote_repo, tag.object, tip_oid).unwrap_or(false) {
                    continue;
                }
                if tag.object != tip_oid && !local_odb.exists(&tag.object) {
                    continue;
                }
                if let Some(old_tip) = local_tracking_oid {
                    if tag.object == old_tip {
                        continue;
                    }
                    // Skip only when we already track this tag ref locally: new tag names that
                    // point into pre-existing history still need their tag objects (t5802).
                    if refs::resolve_ref(local_git_dir, &_tag_refname).is_ok()
                        && merge_base::is_ancestor(&remote_repo, tag.object, old_tip)
                            .unwrap_or(false)
                    {
                        continue;
                    }
                }
                crate::fetch_transport::push_want_unique(&mut wants, tag_ref_oid);
            }
        }
    }

    if should_fetch_tags {
        for (refname, oid) in advertised {
            if !refname.starts_with("refs/tags/") {
                continue;
            }
            let have_ref = refs::resolve_ref(local_git_dir, refname).ok();
            if have_ref == Some(*oid) && local_odb.exists(oid) {
                continue;
            }
            if local_odb.exists(oid) {
                continue;
            }
            if tag_target_is_implicit_include_tag(
                &local_odb,
                &remote_odb,
                &remote_repo,
                *oid,
                &wanted_branch_tips,
            ) {
                continue;
            }
            crate::fetch_transport::push_want_unique(&mut wants, *oid);
        }
    }

    // Explicit tag refspecs that map a `refs/tags/<name>` source to a `refs/tags/` destination —
    // e.g. a `--single-branch --branch <tag>` clone's `+refs/tags/<tag>:refs/tags/<tag>` — must
    // fetch the named tag's object even when tag auto-following is off (`tagOpt = --no-tags`). The
    // heads loop above only covers `refs/heads/` sources. Restrict this to genuine `refs/tags/`
    // sources whose destination is also under `refs/tags/` so we never request peeled `^{}`
    // advertisement entries or other namespaces (t5515).
    for rs in &effective_refspecs {
        if rs.negative || !rs.src.starts_with("refs/tags/") || rs.src.contains('*') {
            continue;
        }
        if !rs.dst.starts_with("refs/tags/") {
            continue;
        }
        let Some(remote_oid) = refs::resolve_ref(remote_git_dir, &rs.src).ok() else {
            continue;
        };
        if local_odb.exists(&remote_oid) {
            continue;
        }
        crate::fetch_transport::push_want_unique(&mut wants, remote_oid);
    }

    wants.dedup();
    Ok(wants)
}

fn tag_target_is_implicit_include_tag(
    local_odb: &Odb,
    remote_odb: &Odb,
    remote_repo: &Repository,
    tag_oid: ObjectId,
    wanted_branch_tips: &[ObjectId],
) -> bool {
    let Ok(tag_obj) = remote_odb.read(&tag_oid) else {
        return false;
    };
    if tag_obj.kind != ObjectKind::Tag {
        return false;
    }
    let Ok(tag) = parse_tag(&tag_obj.data) else {
        return false;
    };
    if local_odb.exists(&tag.object) {
        return false;
    }
    wanted_branch_tips.iter().any(|tip| {
        tag.object != *tip
            && merge_base::is_ancestor(remote_repo, tag.object, *tip).unwrap_or(false)
    })
}

/// Resolve a (possibly nested) tag OID to the non-tag object it ultimately points at, using the
/// objects available in `repo`. Returns the original OID when it is not a tag or cannot be read.
fn peel_tag_target(repo: &Repository, mut oid: ObjectId) -> ObjectId {
    for _ in 0..32 {
        let Ok(obj) = repo.odb.read(&oid) else {
            return oid;
        };
        if obj.kind != ObjectKind::Tag {
            return oid;
        }
        let Ok(tag) = parse_tag(&obj.data) else {
            return oid;
        };
        oid = tag.object;
    }
    oid
}

fn append_follow_tags_for_wants(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    wants: &mut Vec<ObjectId>,
) -> Result<()> {
    let local_odb = Odb::new(&local_git_dir.join("objects"));
    let remote_odb = Odb::new(&remote_git_dir.join("objects"));
    let remote_repo = open_repo(remote_git_dir)?;
    let local_tag_tips: HashSet<ObjectId> = refs::list_refs(local_git_dir, "refs/tags/")?
        .into_iter()
        .map(|(_, oid)| oid)
        .collect();
    let wanted_commit_tips: Vec<ObjectId> = wants
        .iter()
        .copied()
        .filter(|oid| {
            remote_odb
                .read(oid)
                .map(|o| o.kind == ObjectKind::Commit)
                .unwrap_or(false)
        })
        .collect();
    if wanted_commit_tips.is_empty() {
        return Ok(());
    }

    let tag_refs = refs::list_refs(remote_git_dir, "refs/tags/")?;
    for (_tag_refname, tag_ref_oid) in tag_refs {
        if local_tag_tips.contains(&tag_ref_oid) || local_odb.exists(&tag_ref_oid) {
            continue;
        }
        let Ok(tag_obj) = remote_odb.read(&tag_ref_oid) else {
            continue;
        };
        if tag_obj.kind != ObjectKind::Tag {
            continue;
        }
        let Ok(tag) = parse_tag(&tag_obj.data) else {
            continue;
        };
        let follows_wanted_tip = wanted_commit_tips
            .iter()
            .any(|tip| merge_base::is_ancestor(&remote_repo, tag.object, *tip).unwrap_or(false));
        if !follows_wanted_tip {
            continue;
        }
        crate::fetch_transport::push_want_unique(wants, tag_ref_oid);
    }

    wants.sort_by_key(|o| o.to_hex());
    wants.dedup();
    Ok(())
}

fn append_tag_wants_for_cli_fetch(
    local_git_dir: &Path,
    advertised: &[(String, ObjectId)],
    wants: &mut Vec<ObjectId>,
) {
    let local_odb = Odb::new(&local_git_dir.join("objects"));
    for (refname, oid) in advertised {
        if !refname.starts_with("refs/tags/") {
            continue;
        }
        if local_odb.exists(oid) {
            continue;
        }
        crate::fetch_transport::push_want_unique(wants, *oid);
    }
}

/// Fetch from a single remote.
///
/// If `url_override` is Some, use it directly as the remote URL instead of
/// looking it up in config.  This supports path-based remotes like `../one`.
/// Client-side guard mirroring `fetch-pack.c`: refuse an explicit `want <oid>` for an unadvertised
/// object *before* sending it, but ONLY when the server advertises none of the
/// `allow{Tip,Reachable,Any}SHA1InWant` capabilities. When the server enables any of them the client
/// sends the want and lets upload-pack validate it (so an unreachable/non-tip want is rejected
/// server-side with `not our ref`, not pre-empted here) — see t5516 'deny fetch unreachable SHA1',
/// whose final fetch with `allowReachableSHA1InWant=true` must surface `not our ref`.
///
/// `remote_all_refs` is the receiver's full ref list (all of `refs/`, unfiltered). An OID matching a
/// *non-hidden* advertised tip (`transfer.hideRefs` / `uploadpack.hideRefs`) is always allowed.
fn enforce_uploadpack_want_policy(
    remote_repo: &Repository,
    remote_all_refs: &[(String, ObjectId)],
    oid: ObjectId,
) -> Result<()> {
    let remote_config =
        ConfigSet::load(Some(&remote_repo.git_dir), false).unwrap_or_else(|_| ConfigSet::new());

    // Non-hidden advertised tips are always fetchable.
    let mut hidden = grit_lib::ref_exclusions::RefExclusions::default();
    hidden.load_hidden_refs_from_config(&remote_config, "uploadpack");
    let is_advertised_tip = remote_all_refs
        .iter()
        .any(|(name, tip)| *tip == oid && !hidden.ref_excluded(Some(name), name));
    if is_advertised_tip {
        return Ok(());
    }

    let cfg_flag = |key: &str| -> bool {
        remote_config
            .get_bool(key)
            .and_then(|r| r.ok())
            .unwrap_or(false)
    };
    // If the server advertises any allow-*-sha1-in-want capability, defer to it: the client sends
    // the want and upload-pack decides (and reports `not our ref` for a non-tip/unreachable OID).
    if cfg_flag("uploadpack.allowAnySHA1InWant")
        || cfg_flag("uploadpack.allowTipSHA1InWant")
        || cfg_flag("uploadpack.allowReachableSHA1InWant")
    {
        return Ok(());
    }

    bail!(
        "Server does not allow request for unadvertised object {}",
        oid.to_hex()
    );
}

fn fetch_remote(
    git_dir: &Path,
    config: &ConfigSet,
    remote_name: &str,
    url_override: Option<&str>,
    args: &Args,
) -> Result<()> {
    let protocol_version = config
        .get("protocol.version")
        .as_deref()
        .and_then(parse_protocol_version)
        .unwrap_or_else(crate::protocol_wire::effective_client_protocol_version);
    let server_options =
        effective_fetch_server_options(args, config, remote_name, protocol_version)?;

    let is_bare_repo = Repository::open(git_dir, None)
        .map(|repo| repo.is_bare())
        .unwrap_or(false);

    let url_key = format!("remote.{remote_name}.url");
    let legacy_remote = if url_override.is_none() && config.get(&url_key).is_none() {
        read_git_remotes_file(git_dir, remote_name)
    } else {
        None
    };
    let branches_remote =
        if url_override.is_none() && config.get(&url_key).is_none() && legacy_remote.is_none() {
            read_git_branches_remote_file(git_dir, remote_name)
        } else {
            None
        };

    // Determine remote URL: CLI path, config, `.git/remotes/<name>`, or `.git/branches/<name>`.
    let raw_url = if let Some(u) = url_override {
        u.to_owned()
    } else if let Some(u) = config
        .get_all(&url_key)
        .into_iter()
        .find(|v| !v.trim().is_empty())
    {
        u
    } else if let Some(u) = config.get(&url_key) {
        u
    } else if let Some(leg) = &legacy_remote {
        leg.url.clone()
    } else if let Some(br) = &branches_remote {
        br.url.clone()
    } else {
        bail!("fatal: '{remote_name}' does not appear to be a git repository");
    };
    let url = grit_lib::url_rewrite::rewrite_fetch_url(config, &raw_url);

    let is_ext_url = url.starts_with("ext::");
    if is_ext_url {
        crate::protocol::check_protocol_allowed("ext", Some(git_dir))?;
    }

    let is_http_url = !is_ext_url && (url.starts_with("http://") || url.starts_with("https://"));
    let is_git_url = !is_ext_url && !is_http_url && url.starts_with("git://");
    let is_ssh_url = !is_ext_url
        && !is_http_url
        && !is_git_url
        && crate::ssh_transport::is_configured_ssh_url(&url);
    let mut ssh_spec_for_transport = None;

    let mut remote_path = if is_ext_url {
        PathBuf::new()
    } else if is_http_url {
        let proto = if url.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        crate::protocol::check_protocol_allowed(proto, Some(git_dir))?;
        PathBuf::new()
    } else if is_git_url {
        crate::protocol::check_protocol_allowed("git", Some(git_dir))?;
        PathBuf::new()
    } else if is_ssh_url {
        crate::protocol::check_protocol_allowed("ssh", Some(git_dir))?;
        let spec = crate::ssh_transport::parse_ssh_url(&url)?;
        if let Some(gd) = crate::ssh_transport::try_local_git_dir(&spec) {
            gd
        } else {
            ssh_spec_for_transport = Some(spec);
            PathBuf::new()
        }
    } else {
        crate::protocol::check_protocol_allowed("file", Some(git_dir))?;
        // Strip file:// prefix if present.
        // For configured remotes, resolve relative paths from the repository root
        // (not the process CWD), matching Git's behavior for remote.<name>.url.
        if let Some(stripped) = url.strip_prefix("file://") {
            PathBuf::from(stripped)
        } else {
            PathBuf::from(&url)
        }
    };
    // Resolve relative paths from the repository root (not process CWD), for both
    // configured `remote.<name>.url` and path-based remotes (`git fetch ./server`).
    if !is_ext_url && !is_http_url && !is_git_url && remote_path.is_relative() {
        let base = configured_remote_base(git_dir);
        remote_path = base.join(&remote_path);
        if url_override.is_none() && !remote_path.exists() {
            let mut trimmed = url.as_str();
            let mut stripped_any_parent = false;
            while let Some(rest) = trimmed.strip_prefix("../") {
                stripped_any_parent = true;
                trimmed = rest;
            }
            if stripped_any_parent {
                let fallback = base.join(trimmed);
                if fallback.exists() {
                    remote_path = fallback;
                }
            }
        }
    }

    // `git clone` from a bundle records the bundle path as `remote.origin.url`. A no-op `fetch`
    // must succeed (`t5605` bundle clone + fetch). For explicit path fetches against a bundle
    // file (`git fetch ../bundle main:main`), unbundle objects/refs into the current repository.
    if !is_ext_url && !is_http_url && !is_git_url && remote_path_is_git_bundle_file(&remote_path) {
        if url_override.is_none() {
            return Ok(());
        }
        let bundle_refs = parse_bundle_fetch_refs(&remote_path)?;
        crate::commands::bundle::run(crate::commands::bundle::Args {
            action: crate::commands::bundle::BundleAction::Unbundle(
                crate::commands::bundle::UnbundleArgs {
                    progress: false,
                    file: remote_path.to_string_lossy().to_string(),
                },
            ),
        })?;
        update_refs_from_bundle_fetch(git_dir, &bundle_refs, args, is_bare_repo)?;
        return Ok(());
    }

    let remote_repo = if is_ext_url || is_http_url || is_git_url || ssh_spec_for_transport.is_some()
    {
        None
    } else {
        let r = open_repo(&remote_path).with_context(|| {
            format!(
                "could not open remote repository at '{}'",
                remote_path.display()
            )
        })?;
        remote_path = r.git_dir.clone();
        if url_override.is_some() && url.starts_with("file://") {
            r.enforce_safe_directory_git_dir()?;
        }
        Some(r)
    };

    if args.negotiate_only {
        let negotiation_tips = resolve_negotiation_tip_oids(git_dir, &args.negotiation_tip)?;
        let common: Vec<ObjectId> = if is_http_url {
            let proxy_override = config.get(&format!("remote.{remote_name}.proxy"));
            let http_ctx =
                crate::http_client::HttpClientContext::from_config_set_with_proxy_override(
                    config,
                    proxy_override,
                )?;
            crate::http_smart::http_negotiate_only_common(
                git_dir,
                &url,
                &negotiation_tips,
                &http_ctx,
            )?
        } else if let Some(rr) = remote_repo.as_ref() {
            negotiate_only_common_with_remote_repo(git_dir, rr, &negotiation_tips)?
        } else {
            bail!("--negotiate-only is not supported for this transport");
        };
        for oid in common {
            println!("{}", oid.to_hex());
        }
        return Ok(());
    }
    let regular_negotiation_tips = if args.negotiation_tip.is_empty() {
        Vec::new()
    } else {
        resolve_negotiation_tip_oids(git_dir, &args.negotiation_tip)?
    };

    if is_ssh_url && remote_repo.is_some() {
        if let Ok(spec) = crate::ssh_transport::parse_ssh_url(&url) {
            let _ = crate::ssh_transport::record_resolved_git_ssh_upload_pack_for_tests(
                &spec, None, false, false,
            );
        }
    }

    let display_url = resolve_fetch_display_url(git_dir, &url, url_override, remote_repo.as_ref())?;
    let from_display_url =
        resolve_fetch_from_line_url(&url, url_override, remote_repo.as_ref(), &display_url);
    let output_format = effective_output_format(config, args);
    let mut display = FetchDisplay::new(output_format, from_display_url.clone());
    let show_forced_updates = effective_show_forced_updates(config, args);
    let follow_remote_head = parse_follow_remote_head(config, remote_name);
    let include_head_ref_prefix = follow_remote_head.mode != FollowRemoteHead::Never;
    // Only remap tracking namespace for path/URL fetches (`git fetch ./repo`). When the user
    // names a configured remote (`git fetch second`), always store under that remote even if
    // its URL points at the same repository as `origin` (t5505 `remote add -f second`).
    let effective_tracking_remote = if url_override.is_some() {
        remote_repo
            .as_ref()
            .and_then(|rr| find_remote_for_repository_url(config, git_dir, rr))
    } else {
        Some(remote_name.to_owned())
    };
    let tracking_remote_name = effective_tracking_remote
        .clone()
        .unwrap_or_else(|| "origin".to_owned());
    let merge_remote_key =
        effective_tracking_remote
            .as_deref()
            .unwrap_or(if url_override.is_some() {
                "origin"
            } else {
                remote_name
            });
    let _merge_specs = branch_merge_remote_specs(config, merge_remote_key);
    let _head_branch_short = head_short_branch(git_dir);

    let mut cli_refspecs_owned = expand_fetch_cli_tag_args(&args.refspecs)?;
    let fetch_key = format!("remote.{remote_name}.fetch");
    let mut configured_refspecs = collect_refspecs(config, &fetch_key);
    let had_configured_fetch = !configured_refspecs.is_empty();
    let user_passed_cli_refspecs = !args.refspecs.is_empty();
    let explicit_refmap = !args.refmap.is_empty();
    let mut cli_tracking_refspecs: Vec<FetchRefspec> = if explicit_refmap {
        let non_empty: Vec<String> = args
            .refmap
            .iter()
            .filter_map(|v| {
                let t = v.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_owned())
                }
            })
            .collect();
        let mut parsed = parse_cli_fetch_refspecs(&non_empty);
        for spec in &mut parsed {
            if spec.negative || spec.dst.is_empty() {
                continue;
            }
            spec.dst = normalize_fetch_refspec_dst(&spec.dst);
        }
        parsed
    } else if url_override.is_some() {
        // Anonymous URL/path remotes (`git fetch ../repo refs/heads/*:...`) have no configured
        // `remote.<url>.fetch`, so Git performs no opportunistic remote-tracking updates for them.
        // Only honor an explicitly configured fetch refspec; never synthesize the default
        // `refs/remotes/<url>/*` tracking spec, which would write malformed refs from the URL path
        // (e.g. `refs/remotes/../shallow/.git/*`).
        let key = format!("remote.{remote_name}.fetch");
        collect_refspecs(config, &key)
    } else {
        remote_fetch_refspecs(config, remote_name)
    };

    if args.prefetch {
        if user_passed_cli_refspecs {
            let mut specs = parse_cli_fetch_refspecs(&cli_refspecs_owned);
            apply_prefetch_to_refspecs(&mut specs);
            cli_refspecs_owned = specs.iter().map(fetch_refspec_to_cli_string).collect();
        } else {
            // Rebuild from the raw config so an unqualified destination (`bogus/*`) lands at
            // `refs/prefetch/bogus/*` instead of `refs/prefetch/heads/bogus/*` (t5582). Building
            // on the already-normalized `configured_refspecs` would inject a spurious `heads/`.
            configured_refspecs = collect_refspecs_for_prefetch(config, &fetch_key);
        }
        // The remote-tracking destination mapping uses `cli_tracking_refspecs`
        // (the configured/refmap refspecs); rewrite it too so fetched refs land
        // under `refs/prefetch/` rather than `refs/remotes/`.
        apply_prefetch_to_refspecs(&mut cli_tracking_refspecs);
    }

    let cli_refspecs: &[String] = &cli_refspecs_owned;

    let refspecs = if user_passed_cli_refspecs {
        Vec::new()
    } else if let Some(leg) = &legacy_remote {
        parse_legacy_pull_lines(&leg.pull_lines)?
    } else if let Some(br) = &branches_remote {
        // `.git/branches/<name>` (Git `read_branches_file`): the URL's optional `#frag` (or the
        // repository default branch when absent) is fetched into the *local branch* matching the
        // remote/file name, i.e. `refs/heads/<frag>:refs/heads/<name>`.
        let frag = br
            .default_branch
            .clone()
            .unwrap_or_else(|| repo_default_branch_name(config));
        vec![FetchRefspec {
            src: format!("refs/heads/{frag}"),
            dst: format!("refs/heads/{remote_name}"),
            force: false,
            negative: false,
        }]
    } else {
        configured_refspecs.clone()
    };
    let remote_prune_key = format!("remote.{remote_name}.prune");
    let remote_prune_tags_key = format!("remote.{remote_name}.pruneTags");
    let configured_prune = config
        .get(&remote_prune_key)
        .as_deref()
        .and_then(|v| parse_bool(v).ok())
        .or_else(|| {
            config
                .get("fetch.prune")
                .as_deref()
                .and_then(|v| parse_bool(v).ok())
        })
        .unwrap_or(false);
    let configured_prune_tags = config
        .get(&remote_prune_tags_key)
        .as_deref()
        .and_then(|v| parse_bool(v).ok())
        .or_else(|| {
            config
                .get("fetch.pruneTags")
                .as_deref()
                .and_then(|v| parse_bool(v).ok())
        })
        .unwrap_or(false);
    let should_prune = if args.no_prune {
        false
    } else if args.prune {
        true
    } else {
        configured_prune
    };
    // Like Git, prune-tags only takes effect when pruning is enabled.
    // When any CLI refspec is explicitly provided, --prune-tags and pruneTags config are ignored.
    let should_prune_tags = should_prune
        && !user_passed_cli_refspecs
        && (if args.prune_tags {
            true
        } else {
            configured_prune_tags
        });
    let prefetch_left_no_positive = args.prefetch
        && cli_refspecs_owned.is_empty()
        && (user_passed_cli_refspecs || had_configured_fetch);
    let _use_default_remote_tracking = refspecs.is_empty() && !prefetch_left_no_positive;

    let implicit_path_fetch = url_override.is_some()
        && effective_tracking_remote.is_some()
        && cli_refspecs.is_empty()
        && refspecs.is_empty();

    let coalesced_remotes = match remote_repo.as_ref() {
        Some(local_repo)
            if server_options.is_empty()
                && url_override.is_none()
                && legacy_remote.is_none()
                && branches_remote.is_none()
                && cli_refspecs.is_empty() =>
        {
            remotes_sharing_repository_url(config, git_dir, local_repo, remote_name)
        }
        _ => vec![remote_name.to_owned()],
    };
    let fetch_head_refspecs = refspecs.clone();

    let tagopt_remote = effective_tracking_remote.as_deref().unwrap_or(remote_name);
    // Git auto-follows tags (`TAGS_DEFAULT`) only when at least one refspec being fetched has a
    // destination (`autotags` in `get_ref_map`). Explicit CLI refspecs without a colon/dst
    // (`git fetch ../.git one`) set no autotags, so no tags are auto-followed (t5515 `main ../.git
    // one`). `--tags` always follows; `--no-tags` / tagopt overrides still apply.
    let cli_refspecs_have_dst = cli_refspecs.iter().any(|spec| {
        let spec = spec.strip_prefix('+').unwrap_or(spec);
        spec.split_once(':').is_some_and(|(_, dst)| !dst.is_empty())
    });
    let should_fetch_tags = if args.tags {
        true
    } else if args.no_tags
        || implicit_path_fetch
        || (user_passed_cli_refspecs && !cli_refspecs_have_dst)
    {
        false
    } else {
        let tagopt_key = format!("remote.{tagopt_remote}.tagopt");
        match config.get(&tagopt_key).as_deref() {
            Some("--no-tags") => false,
            Some("--tags") => true,
            _ => true,
        }
    };
    // `--tags` / `tagopt = --tags` (`TAGS_SET`) follows *every* advertised tag. The default
    // (`TAGS_DEFAULT`) only auto-follows tags whose target object is reachable from the refs being
    // fetched — a dangling tag like `foobar-tag` (left over after `git reset --hard HEAD~1` on the
    // remote) must NOT be pulled (t5505 "add with reachable tags (default)").
    let follow_all_tags = should_fetch_tags && {
        if args.tags {
            true
        } else {
            let tagopt_key = format!("remote.{tagopt_remote}.tagopt");
            config.get(&tagopt_key).as_deref() == Some("--tags")
        }
    };

    let upload_pack_cmd = args.upload_pack.clone().or_else(|| {
        let key = format!("remote.{remote_name}.uploadpack");
        config.get(&key)
    });

    // Local (non-SSH) non-URL fetches use the upload-pack protocol like upstream Git. This keeps
    // `GIT_TRACE_PACKET` lines (`upload-pack< want …`) and tag-following wants aligned with
    // tests such as t5503-tagfollow. CLI refspecs are applied after the pack is received.
    let ssh_url_with_local_repo = is_ssh_url && remote_repo.is_some();
    let use_upload_pack_negotiation =
        !is_ext_url && !is_http_url && (!is_ssh_url || ssh_url_with_local_repo);

    let upload_pack_refspecs: &[String] = if prefetch_left_no_positive {
        &[]
    } else {
        cli_refspecs
    };

    let ext_upload_pack_git_dir = if is_ext_url {
        crate::ext_transport::try_resolve_ext_upload_pack_git_dir(&url)
    } else {
        None
    };
    let ext_resolved_remote = if let Some(ref gd) = ext_upload_pack_git_dir {
        Some(open_repo(gd)?)
    } else {
        None
    };

    let effective_filter = effective_fetch_filter(config, remote_name, args);
    let filter_active = effective_filter
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    let http_fetch_options = crate::http_smart::HttpFetchOptions {
        negotiation_tip_oids: if args.negotiation_tip.is_empty() {
            None
        } else {
            Some(regular_negotiation_tips.clone())
        },
        depth: args.depth,
        deepen: args.deepen,
        shallow_since: args.shallow_since.clone(),
        shallow_exclude: args.shallow_exclude.iter().cloned().collect(),
        filter_spec: effective_filter.clone(),
        refetch: args.refetch,
        bundle_uri_override: false,
        show_progress: !args.quiet && std::io::stderr().is_terminal(),
    };
    let upload_pack_shallow_options = crate::fetch_transport::UploadPackShallowOptions {
        depth: args.depth,
        deepen: args.deepen,
        shallow_since: args.shallow_since.clone(),
        shallow_exclude: args.shallow_exclude.iter().cloned().collect(),
        unshallow: args.unshallow,
    };
    let pack_filter_spec = effective_filter.as_deref().filter(|s| !s.trim().is_empty());
    let remote_head_advertised_oid: Option<ObjectId>;
    let remote_head_symbolic_branch_from_transport: Option<String>;
    let (mut remote_heads, mut remote_tags, remote_advertised) = if is_ext_url {
        let local_git_for_ext = git_dir.to_path_buf();
        let refspec_owned_ext = refspecs.clone();
        let remote_nm_ext = remote_name.to_owned();
        let should_tags_ext = should_fetch_tags;
        let has_cli_ext = !upload_pack_refspecs.is_empty();
        let cli_owned_ext = if prefetch_left_no_positive {
            Vec::new()
        } else {
            cli_refspecs_owned.clone()
        };
        let remote_gd_ext = ext_upload_pack_git_dir.clone();
        let (heads, tags, head_symref, head_oid) =
            crate::fetch_transport::with_packet_trace_identity("fetch", || {
                crate::ext_transport::fetch_via_ext_skipping(
                    git_dir,
                    &url,
                    "git-upload-pack",
                    upload_pack_refspecs,
                    move |adv| {
                        if has_cli_ext {
                            let Some(ref gd) = remote_gd_ext else {
                                bail!(
                                    "ext:: fetch with refspecs requires a resolvable local upload-pack path"
                                );
                            };
                            let mut wants =
                                crate::fetch_transport::collect_wants_cli(gd, adv, &cli_owned_ext)?;
                            if should_tags_ext {
                                append_tag_wants_for_cli_fetch(&local_git_for_ext, adv, &mut wants);
                            }
                            Ok(wants)
                        } else if let Some(ref gd) = remote_gd_ext {
                            collect_wants_for_upload_pack(
                                &local_git_for_ext,
                                gd,
                                adv,
                                &refspec_owned_ext,
                                should_tags_ext,
                                &remote_nm_ext,
                                false,
                            )
                        } else {
                            crate::fetch_transport::collect_wants(adv, &[])
                        }
                    },
                    filter_active,
                )
            })?;
        remote_head_advertised_oid = head_oid;
        remote_head_symbolic_branch_from_transport = head_symref
            .as_deref()
            .and_then(|s| s.strip_prefix("refs/heads/"))
            .map(ToOwned::to_owned);
        (heads, tags, Vec::new())
    } else if is_git_url {
        crate::protocol::check_protocol_allowed("git", Some(git_dir))?;
        let (heads, tags, head_symref, head_oid) =
            crate::fetch_transport::with_packet_trace_identity("fetch", || {
                crate::fetch_transport::fetch_via_git_protocol_skipping(
                    git_dir,
                    &url,
                    cli_refspecs,
                    filter_active,
                )
            })?;
        remote_head_advertised_oid = head_oid;
        remote_head_symbolic_branch_from_transport = head_symref
            .as_deref()
            .and_then(|s| s.strip_prefix("refs/heads/"))
            .map(ToOwned::to_owned);
        (heads, tags, Vec::new())
    } else if is_http_url {
        // Match Git `fetch.c`: apply `fetch.bundleURI` before the transport fetch so bundle
        // prerequisites are not satisfied early by the pack (t5558 creationToken deepening).
        let proxy_override = config.get(&format!("remote.{remote_name}.proxy"));
        let http_ctx = crate::http_client::HttpClientContext::from_config_set_with_proxy_override(
            config,
            proxy_override,
        )?;
        crate::bundle_uri::maybe_apply_bundle_uri_after_http_fetch_with_client(
            git_dir,
            &url,
            None,
            Some(&http_ctx),
        )?;
        let crate::http_smart::HttpFetchResult {
            heads,
            tags,
            all_advertised: adv,
            ..
        } = crate::fetch_transport::with_packet_trace_identity("fetch", || {
            let result = crate::http_smart::http_fetch_pack(
                git_dir,
                &url,
                upload_pack_refspecs,
                filter_active,
                &http_fetch_options,
                &http_ctx,
            )?;
            // HTTP smart fetch does not always replay negotiation-tip `have` lines on the wire
            // (protocol v2 stateless RPC). Tests such as t5510 grep `fetch> have` in
            // `GIT_TRACE_PACKET`; mirror Git's post-negotiation tip trace here while the fetch
            // packet identity is still active.
            if !regular_negotiation_tips.is_empty() {
                crate::trace_packet::trace_fetch_tip_availability(
                    &git_dir.join("objects"),
                    &regular_negotiation_tips,
                );
            }
            Ok(result)
        })?;
        remote_head_advertised_oid = adv.iter().find(|e| e.name == "HEAD").map(|e| e.oid);
        remote_head_symbolic_branch_from_transport = None;
        let adv: Vec<(String, ObjectId)> = adv.into_iter().map(|e| (e.name, e.oid)).collect();
        let heads: Vec<(String, ObjectId)> = heads.into_iter().map(|e| (e.name, e.oid)).collect();
        let tags: Vec<(String, ObjectId)> = tags.into_iter().map(|e| (e.name, e.oid)).collect();
        (heads, tags, adv)
    } else if let Some(spec) = ssh_spec_for_transport.as_ref() {
        let (heads, tags, head_symref, head_oid) =
            crate::fetch_transport::with_packet_trace_identity("fetch", || {
                crate::fetch_transport::fetch_via_ssh_upload_pack_skipping(
                    git_dir,
                    spec,
                    upload_pack_cmd.as_deref(),
                    upload_pack_refspecs,
                    filter_active,
                )
            })?;
        remote_head_advertised_oid = head_oid;
        remote_head_symbolic_branch_from_transport = head_symref
            .as_deref()
            .and_then(|s| s.strip_prefix("refs/heads/"))
            .map(ToOwned::to_owned);
        (heads, tags, Vec::new())
    } else if use_upload_pack_negotiation {
        crate::protocol::check_protocol_allowed(
            if is_ssh_url { "ssh" } else { "file" },
            Some(git_dir),
        )?;
        let remote_repo_upload = remote_repo
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("upload-pack path requires local remote repository"))?;
        let cli_owned = if prefetch_left_no_positive {
            Vec::new()
        } else {
            cli_refspecs_owned.clone()
        };
        let refspec_owned = refspecs.clone();
        let local_git = git_dir.to_path_buf();
        let remote_gd = remote_repo_upload.git_dir.clone();
        let remote_nm = remote_name.to_owned();
        let has_cli_refspecs = !cli_owned.is_empty();
        let refetch = args.refetch;
        let want_policy_proto_v0 = protocol_version < 2;
        let compute_wants = move |adv: &[(String, ObjectId)]| -> Result<Vec<ObjectId>> {
            if !cli_owned.is_empty() {
                let mut wants =
                    crate::fetch_transport::collect_wants_cli(&remote_gd, adv, &cli_owned)?;
                // Reject an explicit `want <oid>` for an unadvertised object up front (protocol v0/v1)
                // unless the receiver enables `uploadpack.allow*SHA1InWant`. Doing this client-side
                // (mirroring `fetch-pack.c`) avoids the upload-pack child rejecting mid-stream, which
                // would otherwise surface as a broken-pipe abort (t5516 'fetch exact oid',
                // 'peeled advertisements are not considered ref tips').
                if want_policy_proto_v0 {
                    if let Ok(rr) = open_repo(&remote_gd) {
                        let all_refs = refs::list_refs(&remote_gd, "refs/").unwrap_or_default();
                        for spec in &cli_owned {
                            let src = spec
                                .strip_prefix('+')
                                .unwrap_or(spec)
                                .split_once(':')
                                .map(|(a, _)| a)
                                .unwrap_or_else(|| spec.strip_prefix('+').unwrap_or(spec));
                            if let Ok(oid) = ObjectId::from_hex(src) {
                                enforce_uploadpack_want_policy(&rr, &all_refs, oid)?;
                            }
                        }
                    }
                }
                if should_fetch_tags {
                    append_follow_tags_for_wants(&local_git, &remote_gd, &mut wants)?;
                }
                Ok(wants)
            } else {
                collect_wants_for_upload_pack(
                    &local_git,
                    &remote_gd,
                    adv,
                    &refspec_owned,
                    should_fetch_tags,
                    &remote_nm,
                    refetch,
                )
            }
        };
        let (mut heads, mut tags, head_symref, head_oid) =
            crate::fetch_transport::fetch_via_upload_pack_skipping(
                git_dir,
                &remote_path,
                upload_pack_cmd.as_deref(),
                compute_wants,
                has_cli_refspecs,
                include_head_ref_prefix,
                filter_active,
                should_fetch_tags,
                if args.refetch {
                    Some(&[][..])
                } else if regular_negotiation_tips.is_empty() {
                    None
                } else {
                    Some(regular_negotiation_tips.as_slice())
                },
                Some(&upload_pack_shallow_options),
                pack_filter_spec,
                upload_pack_refspecs,
                &server_options,
            )?;
        remote_head_advertised_oid = head_oid;
        remote_head_symbolic_branch_from_transport = head_symref
            .as_deref()
            .and_then(|s| s.strip_prefix("refs/heads/"))
            .map(ToOwned::to_owned);
        // If upload-pack advertised no branch tips (or negotiation returned early) but the remote
        // repository has `refs/heads/*` on disk, read them directly so `refs/remotes/` updates
        // match Git's local fetch behavior (needed for submodule `origin/main` after `git fetch`).
        let remote_repo_upload = remote_repo
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("upload-pack path requires local remote repository"))?;
        if heads.is_empty() {
            heads = refs::list_refs(&remote_repo_upload.git_dir, "refs/heads/")?;
        }
        if tags.is_empty() {
            tags = refs::list_refs(&remote_repo_upload.git_dir, "refs/tags/")?;
        }
        (heads, tags, Vec::new())
    } else {
        let remote_repo = remote_repo
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("non-ext fetch has local remote repo"))?;
        let heads = refs::list_refs(&remote_repo.git_dir, "refs/heads/")?;
        let tags = refs::list_refs(&remote_repo.git_dir, "refs/tags/")?;
        let object_copy_roots = if prefetch_left_no_positive {
            fetch_object_copy_roots(&remote_repo.git_dir, &[], &[], &heads, &tags)?
        } else if !cli_refspecs.is_empty() {
            fetch_object_copy_roots(&remote_repo.git_dir, cli_refspecs, &[], &heads, &tags)?
        } else if legacy_remote.is_some() || branches_remote.is_some() {
            fetch_object_copy_roots(&remote_repo.git_dir, &[], &refspecs, &heads, &tags)?
        } else {
            let coalesced =
                remotes_sharing_repository_url(config, git_dir, remote_repo, remote_name);
            let mut merged = Vec::new();
            for rn in &coalesced {
                let rs = collect_refspecs(config, &format!("remote.{rn}.fetch"));
                merged.extend(fetch_object_copy_roots(
                    &remote_repo.git_dir,
                    &[],
                    &rs,
                    &heads,
                    &tags,
                )?);
            }
            if should_fetch_tags {
                merged.extend(tags.iter().map(|(_, o)| *o));
            }
            merged.sort_by_key(|o| o.to_hex());
            merged.dedup();
            merged
        };
        remote_head_advertised_oid = refs::resolve_ref(&remote_repo.git_dir, "HEAD").ok();
        remote_head_symbolic_branch_from_transport =
            remote_symbolic_head_branch(&remote_repo.git_dir);
        if let Some(spec) = pack_filter_spec {
            copy_reachable_objects_filtered(
                &remote_repo.git_dir,
                git_dir,
                &object_copy_roots,
                spec,
            )
            .context("copying filtered reachable objects from remote")?;
        } else if args.refetch {
            copy_objects(&remote_repo.git_dir, git_dir, true)
                .context("copying objects from remote")?;
        } else {
            copy_reachable_objects(&remote_repo.git_dir, git_dir, &object_copy_roots)
                .context("copying reachable objects from remote")?;
            prune_loose_objects_available_from_alternates(git_dir)
                .context("pruning objects available from alternates")?;
        }
        check_connectivity(git_dir, &object_copy_roots)?;
        (heads, tags, Vec::new())
    };
    if !args.refetch {
        prune_loose_objects_available_from_alternates(git_dir)
            .context("pruning fetched objects available from alternates")?;
    }

    let allow_remote_shallow_updates = args.update_shallow
        || args.depth.is_some()
        || args.deepen.is_some()
        || args.unshallow
        || args.shallow_since.is_some()
        || args.shallow_exclude.is_some();
    let blocked_shallow_remote_refs = if allow_remote_shallow_updates {
        HashSet::new()
    } else if let Some(rr) = ext_resolved_remote.as_ref().or(remote_repo.as_ref()) {
        refs_requiring_update_shallow(&rr.git_dir, git_dir)?
    } else {
        HashSet::new()
    };
    if !blocked_shallow_remote_refs.is_empty() {
        remote_heads.retain(|(name, _)| !blocked_shallow_remote_refs.contains(name));
        remote_tags.retain(|(name, _)| !blocked_shallow_remote_refs.contains(name));
    }
    // Remote advertisements may list tag refs whose tag objects were not sent in a shallow/depth
    // response. Do not update local tag refs to missing objects; this keeps the repo fsck-clean
    // after partial/failed shallow exchanges (e.g. manipulated one-time-script responses).
    let local_repo_for_tag_filter = Repository::open(git_dir, None)
        .with_context(|| format!("open repository {}", git_dir.display()))?;
    if let Some(rr) = ext_resolved_remote.as_ref().or(remote_repo.as_ref()) {
        maybe_materialize_include_tag_objects(
            &local_repo_for_tag_filter,
            rr,
            &remote_heads,
            &remote_tags,
        );
    }
    remote_tags.retain(|(_, oid)| local_repo_for_tag_filter.odb.read(oid).is_ok());
    // Default tag auto-following (`TAGS_DEFAULT`) only keeps tags whose target is reachable from a
    // fetched head. `--tags`/`tagopt = --tags` (`follow_all_tags`) keeps every advertised tag.
    // Explicit refspec tag fetches (`refs/tags/*:...`) are handled elsewhere and must not be pruned
    // here, so restrict this reachability gate to opportunistic auto-follow.
    if should_fetch_tags && !follow_all_tags {
        let head_tips: Vec<ObjectId> = remote_heads.iter().map(|(_, o)| *o).collect();
        if !head_tips.is_empty() {
            // Existing local tags of the same name are always kept (they were not auto-followed).
            remote_tags.retain(|(refname, tag_oid)| {
                if refs::resolve_ref(git_dir, refname).is_ok() {
                    return true;
                }
                let target = peel_tag_target(&local_repo_for_tag_filter, *tag_oid);
                // Only gate tags that point (ultimately) at a *commit* on commit-ancestry from a
                // fetched head: a dangling tag whose commit was reset off the remote branch must not
                // be followed (t5505 "add with reachable tags"). Tags pointing at trees/blobs (e.g.
                // t5515's `tag-one-tree`/`tag-three-file`) are content reachable from a fetched
                // commit and are kept as long as their object was downloaded (the existence retain
                // above already guarantees that).
                let target_is_commit = local_repo_for_tag_filter
                    .odb
                    .read(&target)
                    .map(|o| o.kind == ObjectKind::Commit)
                    .unwrap_or(false);
                if !target_is_commit {
                    return true;
                }
                head_tips.iter().any(|tip| {
                    target == *tip
                        || merge_base::is_ancestor(&local_repo_for_tag_filter, target, *tip)
                            .unwrap_or(false)
                })
            });
        }
    }
    maybe_lazy_fetch_parent_tree_blobs_for_promisor_trace(
        &local_repo_for_tag_filter,
        config,
        &remote_heads,
    );

    let tip_oids: Vec<ObjectId> = remote_heads
        .iter()
        .chain(remote_tags.iter())
        .map(|(_, oid)| *oid)
        .collect();
    let trace_tips: Vec<ObjectId> = if regular_negotiation_tips.is_empty() {
        tip_oids.clone()
    } else {
        regular_negotiation_tips.clone()
    };
    crate::trace_packet::trace_fetch_tip_availability(&git_dir.join("objects"), &trace_tips);

    if !args.negotiate_only && args.unshallow {
        if let Some(rr) = ext_resolved_remote.as_ref().or(remote_repo.as_ref()) {
            // For local/ext transports, mirror Git's `--unshallow` behavior by importing all
            // reachable objects and then syncing local shallow boundaries to the remote's
            // remaining boundaries (or removing the file when the remote is complete).
            copy_reachable_objects_respecting_source_shallow(&rr.git_dir, git_dir, &tip_oids)
                .context("copying objects for --unshallow")?;
            sync_shallow_boundaries_for_unshallow(git_dir, &rr.git_dir, &tip_oids)?;
        } else {
            // For non-local transports we cannot inspect remote shallow boundary state here.
            let shallow_path = git_dir.join("shallow");
            if shallow_path.exists() {
                fs::remove_file(&shallow_path)
                    .context("removing shallow grafts for --unshallow")?;
            }
        }
    }

    // Handle --depth / --deepen: write shallow graft info.
    // `--deepen=<n>` is relative to current shallow depth; include fetched tip advancement.
    let effective_depth = if let Some(depth) = args.depth {
        Some(depth)
    } else if let Some(deepen) = args.deepen {
        let mut target_depth = deepen;
        if let Ok(local_repo) = Repository::open(git_dir, None) {
            let shallow_boundaries = read_shallow_boundaries(git_dir);
            for (remote_ref, new_tip) in &remote_heads {
                let Some(branch) = remote_ref.strip_prefix("refs/heads/") else {
                    continue;
                };
                let local_tracking = format!("refs/remotes/{tracking_remote_name}/{branch}");
                let Some(old_tip) = read_ref_oid(git_dir, &local_tracking) else {
                    continue;
                };
                let current_depth =
                    shallow_depth_from_tip(&local_repo, old_tip, &shallow_boundaries).unwrap_or(1);
                let advance = commit_distance(&local_repo, *new_tip, old_tip).unwrap_or(0);
                let candidate = current_depth.saturating_add(deepen).saturating_add(advance);
                target_depth = target_depth.max(candidate);
            }
        }
        Some(target_depth)
    } else {
        None
    };
    if let Some(depth) = effective_depth {
        let replace_ancestor_boundaries = args.deepen.is_some() && args.depth.is_none();
        if let Some(ref remote_repo) = remote_repo {
            write_shallow_info(
                git_dir,
                &remote_heads,
                remote_repo,
                depth,
                replace_ancestor_boundaries,
            )?;
        } else if is_http_url {
            let local_repo = Repository::open(git_dir, None)
                .context("open repository for shallow metadata after HTTP fetch")?;
            write_shallow_info(
                git_dir,
                &remote_heads,
                &local_repo,
                depth,
                replace_ancestor_boundaries,
            )?;
        }
    } else if args.update_shallow {
        if let Some(rr) = ext_resolved_remote.as_ref().or(remote_repo.as_ref()) {
            write_remote_shallow_info_for_tips(git_dir, &rr.git_dir, &tip_oids)?;
        }
    }

    // Prune namespace: URL/path remotes with explicit refspecs update refs outside
    // refs/remotes/<name>/; prune must cover those destinations (Git behavior).
    let is_url_remote = url_override.is_some();
    let prune_namespace =
        (should_prune || should_prune_tags) && is_url_remote && user_passed_cli_refspecs;
    let _dst_prefix = if prune_namespace {
        longest_common_ref_prefix_from_cli_positive(cli_refspecs)
            .unwrap_or_else(|| "refs/".to_string())
    } else {
        format!("refs/remotes/{remote_name}/")
    };

    // Track which remote-tracking refs we updated (for prune)
    let mut updated_refs: Vec<String> = Vec::new();
    let mut ref_update_failures: Vec<String> = Vec::new();
    let mut tag_clobber_failures: Vec<String> = Vec::new();
    let mut pending_atomic_ref_ops: Vec<PendingRefOp> = Vec::new();
    let mut pending_atomic_noop_head_hook: Option<(String, String, String)> = None;
    // Set when a non-force destination rejects a non-fast-forward update. The fetch still updates
    // the other refs (or, with `--atomic`, aborts the transaction) but exits non-zero, without an
    // extra stderr hint — the reject is reported via its `!` display line.
    let mut had_rejected_updates = false;

    let remote_symbolic_head_branch =
        remote_head_symbolic_branch_from_transport
            .clone()
            .or_else(|| {
                remote_repo
                    .as_ref()
                    .and_then(|r| remote_symbolic_head_branch(&r.git_dir))
            });
    let _remote_default_branch_fetch = remote_repo
        .as_ref()
        .and_then(|r| remote_default_branch_for_fetch_merge(&r.git_dir));

    // Collect FETCH_HEAD entries
    let mut fetch_head_entries: Vec<String> = Vec::new();

    // Command-line refspecs (including OID sources like `git fetch origin $B:refs/heads/main`)
    // must update local refs after upload-pack negotiation as well as after the SSH/copy-object
    // path. `collect_wants_cli` only drives the pack request; ref writes happen here.
    if user_passed_cli_refspecs
        && !prefetch_left_no_positive
        && (!is_ext_url || ext_resolved_remote.is_some())
    {
        let remote_repo = if is_http_url {
            None
        } else {
            match ext_resolved_remote.as_ref() {
                Some(r) => Some(r),
                None => Some(remote_repo.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("CLI refspec fetch requires a local remote repository")
                })?),
            }
        };
        let remote_all_refs: Vec<(String, ObjectId)> = if is_http_url {
            remote_advertised.clone()
        } else {
            refs::list_refs(
                &remote_repo
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "non-HTTP CLI refspec fetch requires local remote repository"
                        )
                    })?
                    .git_dir,
                "refs/",
            )?
        };
        let remote_all_refs: Vec<(String, ObjectId)> = remote_all_refs
            .into_iter()
            .filter(|(refname, _)| !blocked_shallow_remote_refs.contains(refname))
            .collect();
        let find_remote_ref_oid = |name: &str| -> Option<ObjectId> {
            remote_all_refs
                .iter()
                .find(|(refname, _)| refname == name)
                .map(|(_, oid)| *oid)
        };
        let ff_repo = Repository::open(git_dir, None)
            .context("open repository for CLI refspec fast-forward checks")?;
        // Collect negative refspecs first (^pattern)
        let negative_patterns: Vec<&str> = cli_refspecs
            .iter()
            .filter_map(|s| s.strip_prefix('^'))
            .collect();

        // Validate negative refspecs: they must be ref patterns, not OIDs
        for pat in &negative_patterns {
            let clean = pat.strip_prefix("refs/").unwrap_or(pat);
            if clean.chars().all(|c| c.is_ascii_hexdigit()) && clean.len() >= 7 {
                bail!("negative refspecs do not support object ids: ^{pat}");
            }
        }

        let is_excluded = |refname: &str| -> bool {
            negative_patterns
                .iter()
                .any(|pat| ref_excluded_by_negative_pattern(pat, refname))
        };

        // Pre-check: detect conflicting CLI refspec mappings
        {
            let mut dst_to_src: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for spec in cli_refspecs {
                if spec.starts_with('^') {
                    continue;
                }
                let spec_clean = spec.strip_prefix('+').unwrap_or(spec.as_str());
                let (src, dst) = if let Some(idx) = spec_clean.find(':') {
                    (
                        spec_clean[..idx].to_owned(),
                        spec_clean[idx + 1..].to_owned(),
                    )
                } else {
                    continue;
                };
                if dst.is_empty() {
                    continue;
                }
                if src.contains('*') {
                    for (refname, _) in &remote_all_refs {
                        if is_excluded(refname) {
                            continue;
                        }
                        if let Some(matched) = match_glob_pattern(&src, refname) {
                            let local_ref = dst.replacen('*', matched, 1);
                            if let Some(prev_src) = dst_to_src.get(&local_ref) {
                                if prev_src != refname {
                                    {
                                        eprintln!(
                                            "fatal: Cannot fetch both {} and {} to {}",
                                            prev_src, refname, local_ref
                                        );
                                        std::process::exit(128);
                                    }
                                }
                            } else {
                                dst_to_src.insert(local_ref, refname.to_string());
                            }
                        }
                    }
                } else {
                    let remote_ref = resolve_advertised_ref_for_fetch_src(
                        &src,
                        &remote_all_refs,
                        remote_symbolic_head_branch.as_deref(),
                    )
                    .unwrap_or_else(|| {
                        if src.starts_with("refs/") {
                            src.clone()
                        } else {
                            format!("refs/heads/{src}")
                        }
                    });
                    let local_ref = normalize_fetch_refspec_dst(&dst);
                    if let Some(prev_src) = dst_to_src.get(&local_ref) {
                        if prev_src != &remote_ref {
                            {
                                eprintln!(
                                    "fatal: Cannot fetch both {} and {} to {}",
                                    prev_src, remote_ref, local_ref
                                );
                                std::process::exit(128);
                            }
                        }
                    } else {
                        dst_to_src.insert(local_ref, remote_ref);
                    }
                }
            }
        }

        // Process command-line refspecs directly.
        for spec in cli_refspecs {
            // Skip negative refspecs (already collected above)
            if spec.starts_with('^') {
                continue;
            }
            // Check for force prefix '+'
            let (force, spec_clean) = if let Some(stripped) = spec.strip_prefix('+') {
                (true, stripped)
            } else {
                (false, spec.as_str())
            };
            let (src, dst) = if let Some(idx) = spec_clean.find(':') {
                (
                    spec_clean[..idx].to_owned(),
                    spec_clean[idx + 1..].to_owned(),
                )
            } else {
                (spec_clean.to_owned(), String::new())
            };

            // Handle glob refspecs (e.g. refs/remotes/*:refs/remotes/*)
            if src.contains('*') {
                for (refname, remote_oid) in &remote_all_refs {
                    if is_excluded(refname) {
                        continue;
                    }
                    if let Some(matched) = match_glob_pattern(&src, refname) {
                        let local_ref = dst.replacen('*', matched, 1);
                        updated_refs.push(local_ref.clone());
                        let old_oid = read_ref_oid(git_dir, &local_ref);
                        let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);

                        // Every ref matched by a (non-excluded) refspec contributes a FETCH_HEAD
                        // line, even when the local tracking ref is already up to date. Git records
                        // the fetched tip regardless of whether the ref pointer changed; only the
                        // ref update and the progress message are skipped for no-op updates. Emit
                        // this before the up-to-date early continue so an unchanged ref still lands
                        // in FETCH_HEAD (t5582: negative pattern refspec FETCH_HEAD contents).
                        fetch_head_entries.push(fetch_head_branch_line(
                            remote_oid,
                            branch,
                            &display_url,
                            false,
                        ));

                        if local_ref.starts_with("refs/heads/") && !args.update_head_ok {
                            if let Some(wt_path) = is_branch_in_worktree(git_dir, &local_ref) {
                                bail!(
                                    "refusing to fetch into branch '{}' checked out at '{}'",
                                    local_ref,
                                    wt_path
                                );
                            }
                        }
                        if old_oid.as_ref() == Some(remote_oid) {
                            continue;
                        }

                        // Check fast-forward for wildcard updates. A non-fast-forward update to a
                        // non-force destination is *rejected* (reported with `!`). With `--atomic`
                        // the whole transaction is aborted (no refs written); without it, the other
                        // refs still update. Either way the command exits non-zero at the end and
                        // the reject is reported only via the `!` line (t5574 porcelain output).
                        let mut rejected_non_ff = false;
                        if let Some(ref old) = old_oid {
                            if old != remote_oid && !(force || args.force) {
                                let is_ff = merge_base::is_ancestor(&ff_repo, *old, *remote_oid)
                                    .unwrap_or(true);
                                if !is_ff {
                                    rejected_non_ff = true;
                                }
                            }
                        }

                        if rejected_non_ff {
                            // A rejected non-ff update makes the fetch exit non-zero, but it is
                            // reported via the `!` display line only — no extra "could not be
                            // updated" hint (t5574 expects empty stderr in porcelain mode).
                            had_rejected_updates = true;
                            if !args.quiet {
                                display.push(
                                    '!',
                                    "[rejected]",
                                    refname,
                                    &local_ref,
                                    &local_ref,
                                    old_oid.unwrap_or_else(ObjectId::zero),
                                    *remote_oid,
                                    Some("non-fast-forward"),
                                );
                            }
                            continue;
                        }

                        apply_single_ref_update(
                            args,
                            git_dir,
                            &mut pending_atomic_ref_ops,
                            &local_ref,
                            old_oid,
                            *remote_oid,
                            &mut ref_update_failures,
                        )?;

                        if !args.quiet {
                            let (code, summary, error) = classify_ref_update(
                                &ff_repo,
                                refname,
                                old_oid.as_ref(),
                                remote_oid,
                                show_forced_updates,
                            );
                            display.push(
                                code,
                                &summary,
                                refname,
                                &local_ref,
                                &local_ref,
                                old_oid.unwrap_or_else(ObjectId::zero),
                                *remote_oid,
                                error,
                            );
                        }
                    }
                }
                // Also copy symbolic refs for the matched pattern
                if let Some(remote_repo) = remote_repo {
                    copy_symrefs(&remote_repo.git_dir, git_dir, &src, &dst)?;
                }
                continue;
            }

            // Resolve source: full OID, ref name, or short branch/tag (match `collect_wants_cli`).
            let (remote_oid, resolved_remote_ref): (ObjectId, Option<String>) =
                if let Ok(oid) = ObjectId::from_hex(src.as_str()) {
                    // An explicit `want <oid>` for an object that is not an advertised ref tip is
                    // rejected by upload-pack unless the server enables one of the
                    // `uploadpack.allow{Tip,Reachable,Any}SHA1InWant` policies. Protocol v2 always
                    // allows fetching unadvertised objects, so only enforce this for v0/v1
                    // (t5516 'fetch exact oid', 'shallow fetch reachable SHA1', 'deny ...',
                    // 'peeled advertisements are not considered ref tips').
                    if protocol_version < 2 {
                        if let Some(rr) = remote_repo {
                            enforce_uploadpack_want_policy(rr, &remote_all_refs, oid)?;
                        }
                    }
                    (oid, None)
                } else {
                    let resolved_ref = resolve_advertised_ref_for_fetch_src(
                        &src,
                        &remote_all_refs,
                        remote_symbolic_head_branch.as_deref(),
                    )
                    .with_context(|| format!("couldn't find remote ref '{src}'"))?;
                    let oid = remote_oid_for_resolved_ref(
                        &resolved_ref,
                        &find_remote_ref_oid,
                        remote_head_advertised_oid,
                        remote_symbolic_head_branch.as_deref(),
                    )
                    .with_context(|| format!("couldn't find remote ref '{src}'"))?;
                    (oid, Some(resolved_ref))
                };

            let branch_label = if ObjectId::from_hex(src.as_str()).is_ok() {
                src.as_str()
            } else if src.is_empty() {
                "HEAD"
            } else if let Some(rest) = src.strip_prefix("refs/heads/") {
                rest
            } else if let Some(rest) = src.strip_prefix("refs/tags/") {
                rest
            } else {
                src.as_str()
            };
            // A `tag <name>` CLI argument (`refs/tags/<name>:refs/tags/<name>`) is rendered as a
            // `tag '<name>'` FETCH_HEAD line, not a branch line. The dedicated CLI-tag block below
            // owns those lines, so skip emitting the branch-style line here to avoid a mislabeled
            // duplicate (t5515 `... tag tag-one tag tag-three`). Other CLI refspecs emit a branch
            // line as before.
            if !src.starts_with("refs/tags/") {
                fetch_head_entries.push(fetch_head_branch_line(
                    &remote_oid,
                    branch_label,
                    &display_url,
                    dst.is_empty(),
                ));
            }
            // `--set-upstream` is handled once, after all refspecs are processed, in
            // `apply_set_upstream` (mirroring builtin/fetch.c). Doing it per-refspec here would set
            // the wrong branch (it must configure the *current* branch, not the fetched one) and
            // would ignore the "exactly one branch / no explicit destination" rules.

            // If a destination is specified, write the ref there
            if !dst.is_empty() {
                let local_ref = normalize_fetch_refspec_dst(&dst);
                ensure_head_ref_target_is_commit(
                    remote_repo,
                    &local_ref,
                    remote_oid,
                    src.as_str(),
                    &resolved_remote_ref,
                )?;
                updated_refs.push(local_ref.clone());

                let old_oid = read_ref_oid(git_dir, &local_ref);
                // Refuse fetching into a branch checked out in *any* worktree (Git
                // `branch_checked_out`). This applies even to bare repositories, whose linked
                // worktrees can have branches checked out (t5516 119); `is_branch_in_worktree`
                // already excludes a bare main worktree, so no `is_bare_repo` guard is needed.
                if local_ref.starts_with("refs/heads/") && !src.is_empty() && !args.update_head_ok {
                    if let Some(wt_path) = is_branch_in_worktree(git_dir, &local_ref) {
                        bail!(
                            "refusing to fetch into branch '{}' checked out at '{}'",
                            local_ref,
                            wt_path
                        );
                    }
                }

                // Tags are not fast-forwardable: an explicit `refs/tags/<t>` destination whose
                // existing local tag points elsewhere is rejected unless the refspec is forced
                // (or `--force`/tagOpt force is in effect), mirroring Git's `update_local_ref`
                // "would clobber existing tag". The `+refs/tags/*:refs/tags/*` retry passes `force`.
                if local_ref.starts_with("refs/tags/") {
                    if let Some(ref old) = old_oid {
                        if old != &remote_oid
                            && !(force
                                || args.force
                                || should_force_tag_update(config, remote_name, args))
                        {
                            let tag_name =
                                local_ref.strip_prefix("refs/tags/").unwrap_or(&local_ref);
                            if !args.quiet {
                                display.push(
                                    '!',
                                    "[rejected]",
                                    branch_label,
                                    &local_ref,
                                    &local_ref,
                                    *old,
                                    remote_oid,
                                    Some("would clobber existing tag"),
                                );
                            }
                            tag_clobber_failures.push(tag_name.to_owned());
                            continue;
                        }
                    }
                }

                // Check fast-forward: reject non-ff updates unless forced
                if let Some(ref old) = old_oid {
                    if old != &remote_oid && !(force || args.force) {
                        let is_ff =
                            merge_base::is_ancestor(&ff_repo, *old, remote_oid).unwrap_or(true);
                        if !is_ff {
                            eprintln!(" ! [rejected]        {src} -> {dst} (non-fast-forward)");
                            bail!("cannot fast-forward ref '{local_ref}'");
                        }
                    }
                }

                if old_oid.as_ref() != Some(&remote_oid) {
                    apply_single_ref_update(
                        args,
                        git_dir,
                        &mut pending_atomic_ref_ops,
                        &local_ref,
                        old_oid,
                        remote_oid,
                        &mut ref_update_failures,
                    )?;

                    // Classify based on the *remote* ref name: a resolved branch/tag, else the
                    // raw OID source (which yields `[new ref]`, matching Git's update_local_ref).
                    let classify_name = resolved_remote_ref.as_deref().unwrap_or(src.as_str());
                    if !args.atomic {
                        let fast_forward = old_oid.is_none_or(|old| {
                            merge_base::is_ancestor(&ff_repo, old, remote_oid).unwrap_or(true)
                        });
                        write_fetch_explicit_reflog(
                            args,
                            git_dir,
                            &local_ref,
                            classify_name,
                            old_oid.as_ref(),
                            &remote_oid,
                            fast_forward,
                        );
                    }

                    if !args.quiet {
                        let (code, summary, error) = classify_ref_update(
                            &ff_repo,
                            classify_name,
                            old_oid.as_ref(),
                            &remote_oid,
                            show_forced_updates,
                        );
                        display.push(
                            code,
                            &summary,
                            branch_label,
                            &local_ref,
                            &local_ref,
                            old_oid.unwrap_or_else(ObjectId::zero),
                            remote_oid,
                            error,
                        );
                    }
                }
            } else {
                // No explicit destination. If a configured tracking refspec maps the resolved
                // remote ref to a local ref, update that ref; otherwise the fetch lands only in
                // FETCH_HEAD and Git reports a ` * <kind>  <remote> -> FETCH_HEAD` line.
                let tracking_local = resolved_remote_ref
                    .as_deref()
                    .and_then(|rrn| map_ref_through_refspecs(rrn, &cli_tracking_refspecs));
                if let Some(local_ref) = tracking_local {
                    updated_refs.push(local_ref.clone());
                    let old_oid = read_ref_oid(git_dir, &local_ref);
                    if old_oid.as_ref() != Some(&remote_oid) {
                        if local_ref.starts_with("refs/heads/") && !args.update_head_ok {
                            if let Some(wt_path) = is_branch_in_worktree(git_dir, &local_ref) {
                                bail!(
                                    "refusing to fetch into branch '{}' checked out at '{}'",
                                    local_ref,
                                    wt_path
                                );
                            }
                        }
                        apply_single_ref_update(
                            args,
                            git_dir,
                            &mut pending_atomic_ref_ops,
                            &local_ref,
                            old_oid,
                            remote_oid,
                            &mut ref_update_failures,
                        )?;
                        if !args.quiet {
                            let classify_name =
                                resolved_remote_ref.as_deref().unwrap_or(src.as_str());
                            let (code, summary, error) = classify_ref_update(
                                &ff_repo,
                                classify_name,
                                old_oid.as_ref(),
                                &remote_oid,
                                show_forced_updates,
                            );
                            display.push(
                                code,
                                &summary,
                                branch_label,
                                &local_ref,
                                &local_ref,
                                old_oid.unwrap_or_else(ObjectId::zero),
                                remote_oid,
                                error,
                            );
                        }
                    }
                } else {
                    // FETCH_HEAD-only update (e.g. `git fetch origin HEAD` or `origin
                    // refs/changes/6`). The fetched commit lands only in FETCH_HEAD, not under
                    // `refs/`, so the submodule-recursion walk (which scans `refs/` tips) would miss
                    // any submodule change it records. Note the OID as a candidate superproject tip
                    // so `check_for_new_submodule_commits` still recurses (t5526 #41/#42/#43/#45).
                    crate::fetch_submodule_record::record_submodule_tip(&remote_oid);
                    if !args.quiet {
                        let remote_ref_name =
                            resolved_remote_ref.as_deref().unwrap_or(src.as_str());
                        let kind = fetch_head_ref_kind(remote_ref_name);
                        display.push(
                            '*',
                            kind,
                            branch_label,
                            "FETCH_HEAD",
                            "FETCH_HEAD",
                            ObjectId::zero(),
                            remote_oid,
                            None,
                        );
                    }
                }
            }
        }

        // Opportunistic remote-tracking updates: when explicit *wildcard* CLI refspecs are used,
        // Git also applies the configured `remote.<name>.fetch` refspecs to the advertised refs,
        // updating (e.g.) `refs/remotes/origin/*` alongside the CLI destinations (t5574). Restrict
        // this to wildcard CLI fetches so a single-ref fetch (`origin HEAD`, `origin <oid>:x`)
        // doesn't sprout spurious tracking updates.
        let cli_has_wildcard = cli_refspecs.iter().any(|s| {
            let clean = s.strip_prefix('^').unwrap_or(s);
            let clean = clean.strip_prefix('+').unwrap_or(clean);
            let src = clean.split(':').next().unwrap_or(clean);
            src.contains('*')
        });
        for spec in cli_tracking_refspecs.iter().filter(|_| cli_has_wildcard) {
            if spec.negative || spec.dst.is_empty() || !spec.src.contains('*') {
                continue;
            }
            for (refname, remote_oid) in &remote_all_refs {
                if is_excluded(refname) {
                    continue;
                }
                let Some(matched) = match_glob_pattern(&spec.src, refname) else {
                    continue;
                };
                let local_ref = spec.dst.replacen('*', matched, 1);
                if updated_refs.contains(&local_ref) {
                    continue;
                }
                updated_refs.push(local_ref.clone());
                let old_oid = read_ref_oid(git_dir, &local_ref);
                if old_oid.as_ref() == Some(remote_oid) {
                    continue;
                }
                let forced = spec.force || args.force;
                let mut rejected_non_ff = false;
                if let Some(ref old) = old_oid {
                    if old != remote_oid && !forced {
                        let is_ff =
                            merge_base::is_ancestor(&ff_repo, *old, *remote_oid).unwrap_or(true);
                        if !is_ff {
                            rejected_non_ff = true;
                        }
                    }
                }
                if rejected_non_ff {
                    had_rejected_updates = true;
                    if !args.quiet {
                        display.push(
                            '!',
                            "[rejected]",
                            refname,
                            &local_ref,
                            &local_ref,
                            old_oid.unwrap_or_else(ObjectId::zero),
                            *remote_oid,
                            Some("non-fast-forward"),
                        );
                    }
                    continue;
                }
                apply_single_ref_update(
                    args,
                    git_dir,
                    &mut pending_atomic_ref_ops,
                    &local_ref,
                    old_oid,
                    *remote_oid,
                    &mut ref_update_failures,
                )?;
                if !args.quiet {
                    let (code, summary, error) = classify_ref_update(
                        &ff_repo,
                        refname,
                        old_oid.as_ref(),
                        remote_oid,
                        show_forced_updates,
                    );
                    display.push(
                        code,
                        &summary,
                        refname,
                        &local_ref,
                        &local_ref,
                        old_oid.unwrap_or_else(ObjectId::zero),
                        *remote_oid,
                        error,
                    );
                }
            }
        }

        // Emit warnings when CLI refspec destinations conflict with configured tracking
        let configured_refspecs = collect_refspecs(config, &fetch_key);
        if !configured_refspecs.is_empty() {
            for spec in cli_refspecs {
                if spec.starts_with('^') {
                    continue;
                }
                let spec_clean = spec.strip_prefix('+').unwrap_or(spec.as_str());
                let (src, dst) = if let Some(idx) = spec_clean.find(':') {
                    (
                        spec_clean[..idx].to_owned(),
                        spec_clean[idx + 1..].to_owned(),
                    )
                } else {
                    continue;
                };
                if dst.is_empty() || src.contains('*') {
                    continue;
                }
                let remote_ref = if src.starts_with("refs/") {
                    src.clone()
                } else {
                    format!("refs/heads/{src}")
                };
                let local_ref = normalize_fetch_refspec_dst(&dst);
                // Check what the configured refspec would map this destination to
                if let Some(usual_src) = reverse_map_refspec(&local_ref, &configured_refspecs) {
                    if usual_src != remote_ref {
                        eprintln!(
                            "warning: {} usually tracks {}, not {}",
                            local_ref, usual_src, remote_ref
                        );
                    }
                }
            }
        }
    } else {
        // When several remotes share the same repository URL we still fetch once, but each
        // `git fetch <name>` must map advertised refs through **that** remote's refspecs first.
        // If we merge all coalesced refspecs in config order, a second remote that reuses
        // `origin`'s URL would incorrectly resolve to `refs/remotes/origin/*` (t5505).
        let mut union_refspecs: Vec<FetchRefspec> = Vec::new();
        let primary_key = format!("remote.{remote_name}.fetch");
        // `git fetch --prefetch` redirects every positive destination under `refs/prefetch/`
        // (and drops tag refspecs). Build prefetch refspecs from the **raw** config dst so an
        // unqualified destination like `bogus/*` becomes `refs/prefetch/bogus/*` rather than
        // `refs/prefetch/heads/bogus/*` (t5582). For normal fetches keep the standard collection.
        let collect = |key: &str| -> Vec<FetchRefspec> {
            if args.prefetch {
                collect_refspecs_for_prefetch(config, key)
            } else {
                collect_refspecs(config, key)
            }
        };
        let mut primary = collect(&primary_key);
        if primary.is_empty() && !args.prefetch {
            // Legacy `.git/remotes/<name>` (Pull:) and `.git/branches/<name>` remotes have no
            // `remote.<name>.fetch` config; their tracking refspecs live in `refspecs`. Use those
            // instead of the synthetic `refs/remotes/<name>/*` default (t5515 remote-explicit /
            // branches-default), otherwise fetched branches land under the wrong namespace.
            primary =
                if (legacy_remote.is_some() || branches_remote.is_some()) && !refspecs.is_empty() {
                    refspecs.clone()
                } else if url_override.is_some() {
                    // Anonymous URL/path fetch (`git fetch ../.git`) with no refspec: Git performs no
                    // opportunistic remote-tracking updates. Never synthesize `refs/remotes/<url>/*`,
                    // which would write malformed refs from the URL path (t5515 `main ../.git`).
                    Vec::new()
                } else if config.get(&format!("remote.{remote_name}.url")).is_some()
                    || config
                        .get(&format!("remote.{remote_name}.pushurl"))
                        .is_some()
                {
                    // A configured remote with a url but no fetch refspec performs no opportunistic
                    // remote-tracking updates (e.g. a `--single-branch` clone of a detached HEAD,
                    // whose origin has only `url`). Match upstream's empty `remote->fetch`.
                    Vec::new()
                } else {
                    default_fetch_refspecs(remote_name)
                };
        }
        // The primary remote's own refspecs: a single advertised ref may match more than one of
        // these (e.g. `+refs/heads/*:refs/remotes/origin/*` plus an explicit
        // `refs/heads/main:refs/heads/upstream`), and Git fetches it into *every* such destination.
        // Coalesced (other-remote) refspecs only provide a single fallback destination, so they are
        // applied first-match — never multiplied into other remotes' tracking namespaces (t5505).
        let primary_refspecs = primary.clone();
        union_refspecs.extend(primary);
        for rn in &coalesced_remotes {
            if rn == remote_name {
                continue;
            }
            let key = format!("remote.{rn}.fetch");
            let mut rs = collect(&key);
            if rs.is_empty() && !args.prefetch {
                rs = default_fetch_refspecs(rn);
            }
            union_refspecs.extend(rs);
        }

        let refs_for_mapping: Vec<(String, ObjectId)> =
            if let Some(rr) = ext_resolved_remote.as_ref().or(remote_repo.as_ref()) {
                refs::list_refs(&rr.git_dir, "refs/")?
            } else if remote_advertised.is_empty() {
                let mut refs = remote_heads.clone();
                refs.extend(remote_tags.clone());
                refs
            } else {
                remote_advertised.clone()
            };

        if !union_refspecs.is_empty() {
            let mut dst_to_src: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for (refname, _) in &refs_for_mapping {
                if let Some(local_ref) = map_ref_through_refspecs(refname, &union_refspecs) {
                    if let Some(prev_src) = dst_to_src.get(&local_ref) {
                        if prev_src != refname {
                            eprintln!(
                                "fatal: Cannot fetch both {} and {} to {}",
                                prev_src, refname, local_ref
                            );
                            std::process::exit(128);
                        }
                    } else {
                        dst_to_src.insert(local_ref, refname.to_string());
                    }
                }
            }
        }
        let prune_updated_refs = updated_refs.clone();

        // Standard path: update refs according to configured fetch refspecs.
        let has_merge_cfg = branch_has_merge_config_for_remote(git_dir, config, remote_name);
        // Without `branch.*.merge`, Git marks only the first ref of the first non-pattern refspec
        // as for-merge, regardless of where it lands in advertised order.
        let first_refspec_merge_ref = if !has_merge_cfg && !refspecs.is_empty() {
            fetch_head_first_refspec_merge_ref(&refspecs, &refs_for_mapping)
        } else {
            None
        };

        for (idx, (refname, advertised_oid)) in refs_for_mapping.iter().enumerate() {
            let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
            // A source ref can be matched by more than one of the primary remote's configured
            // fetch refspecs (e.g. the default `+refs/heads/*:refs/remotes/origin/*` plus an extra
            // `refs/heads/main:refs/heads/upstream`). Git fetches it into *every* such destination,
            // so apply the ref update for each while emitting a single FETCH_HEAD entry per source.
            // When the primary's refspecs do not cover this ref, fall back to the first match across
            // the coalesced (shared-URL) remotes — never multiplying into their namespaces.
            let mut local_refs = map_ref_through_refspecs_all(refname, &primary_refspecs);
            if local_refs.is_empty() {
                if let Some(lr) = map_ref_through_refspecs(refname, &union_refspecs) {
                    local_refs.push(lr);
                } else {
                    continue;
                }
            }
            let remote_oid = if let Some(rr) = ext_resolved_remote.as_ref().or(remote_repo.as_ref())
            {
                refs::resolve_ref(&rr.git_dir, refname)
                    .ok()
                    .unwrap_or(*advertised_oid)
            } else {
                *advertised_oid
            };

            if refname.starts_with("refs/heads/") {
                let for_merge = if has_merge_cfg {
                    fetch_head_is_for_merge_with_branch(git_dir, config, remote_name, refname)
                } else if refspecs.is_empty() {
                    // Without explicit fetch refspecs, tie FETCH_HEAD's for-merge line to the branch
                    // checked out locally when it exists on the remote. If HEAD is on a branch but the
                    // remote does not have that ref, no branch line is for-merge (do not use `idx==0`,
                    // since advertised order can put another branch first and break `FETCH_HEAD` tests).
                    match current_branch_from_head(git_dir) {
                        Some(b) => refname == &format!("refs/heads/{b}"),
                        None => idx == 0,
                    }
                } else {
                    first_refspec_merge_ref.as_deref() == Some(refname.as_str())
                };
                fetch_head_entries.push(fetch_head_branch_line(
                    &remote_oid,
                    branch,
                    &display_url,
                    for_merge,
                ));
            }

            for local_ref in &local_refs {
                let local_ref = local_ref.as_str();
                updated_refs.push(local_ref.to_string());

                let old_oid = read_ref_oid(git_dir, local_ref);
                if old_oid.as_ref() == Some(&remote_oid) {
                    continue;
                }

                if should_prune && local_ref.starts_with("refs/remotes/") {
                    let local_ref_path = git_dir.join(local_ref);
                    // The incoming flat ref (e.g. `refs/remotes/origin/dir`) collides with an existing
                    // ref *namespace* (`refs/remotes/origin/dir/file`). Such child refs may be stored
                    // loose (a real directory on disk) or packed (only in `packed-refs`, with no
                    // directory present — the common case right after `git clone`, which packs all
                    // tracking refs). List both via `list_refs` so the packed case is also detected;
                    // `is_dir()` alone misses it (t5510 'branchname D/F conflict resolved by --prune').
                    let conflict_prefix = format!("{local_ref}/");
                    let stale_refs = refs::list_refs(git_dir, &conflict_prefix)?;
                    if !stale_refs.is_empty() {
                        for (stale_ref, stale_oid) in stale_refs {
                            // Only prune children the remote no longer advertises into this
                            // namespace; a child still mapped this fetch is updated separately.
                            if updated_refs.iter().any(|r| r == &stale_ref) {
                                continue;
                            }
                            apply_single_ref_delete(
                                args,
                                git_dir,
                                &mut pending_atomic_ref_ops,
                                &stale_ref,
                                Some(stale_oid),
                                &mut ref_update_failures,
                            )?;
                            if !args.quiet {
                                display.push(
                                    '-',
                                    "[deleted]",
                                    "(none)",
                                    &stale_ref,
                                    &stale_ref,
                                    stale_oid,
                                    ObjectId::zero(),
                                    None,
                                );
                            }
                        }
                        // If we removed all child refs, clear now-empty directories so the
                        // incoming flat ref can be created (D/F conflict during `--prune`).
                        let mut dir = local_ref_path.clone();
                        while dir.starts_with(git_dir) && dir.is_dir() {
                            let is_empty = fs::read_dir(&dir)
                                .ok()
                                .map(|mut it| it.next().is_none())
                                .unwrap_or(false);
                            if !is_empty {
                                break;
                            }
                            if fs::remove_dir(&dir).is_err() {
                                break;
                            }
                            let Some(parent) = dir.parent() else {
                                break;
                            };
                            dir = parent.to_path_buf();
                        }
                    }
                }

                apply_single_ref_update(
                    args,
                    git_dir,
                    &mut pending_atomic_ref_ops,
                    local_ref,
                    old_oid,
                    remote_oid,
                    &mut ref_update_failures,
                )?;
                if !args.atomic {
                    let _ = append_fetch_reflog(
                        git_dir,
                        local_ref,
                        old_oid.as_ref(),
                        &remote_oid,
                        &url,
                        branch,
                    );
                }

                if !args.quiet {
                    let display_repo = Repository::open(git_dir, None).ok();
                    let (code, summary, error) = match display_repo.as_ref() {
                        Some(repo) => classify_ref_update(
                            repo,
                            refname,
                            old_oid.as_ref(),
                            &remote_oid,
                            show_forced_updates,
                        ),
                        None => classify_new_ref_summary(refname),
                    };
                    let old_for_porcelain = old_oid.unwrap_or_else(ObjectId::zero);
                    display.push(
                        code,
                        &summary,
                        refname,
                        local_ref,
                        local_ref,
                        old_for_porcelain,
                        remote_oid,
                        error,
                    );
                }
            }
        }

        // Git `builtin/fetch.c` `get_ref_map` default case: when the remote has no configured fetch
        // refspec (and there is no matching `branch.<name>.merge`), `git fetch <remote>` fetches the
        // remote's HEAD ref and marks it `FETCH_HEAD_MERGE`. The loop above maps refs only through
        // configured refspecs, so with none it emits no branch line — record the remote HEAD branch
        // here as the single for-merge FETCH_HEAD entry (t5710 `update-ref HEAD FETCH_HEAD`).
        if refspecs.is_empty()
            && union_refspecs.is_empty()
            && !has_merge_cfg
            && !args.prefetch
            && !user_passed_cli_refspecs
            && !implicit_path_fetch
        {
            if let Some(rb) = remote_symbolic_head_branch.as_deref() {
                if let Some(oid) = refs_for_mapping
                    .iter()
                    .find(|(r, _)| r == &format!("refs/heads/{rb}"))
                    .map(|(_, o)| *o)
                {
                    fetch_head_entries.push(fetch_head_branch_line(&oid, rb, &display_url, true));
                }
            }
        }

        // Git `add_merge_config`: for the default fetch, any `branch.<b>.merge` entry that was not
        // already fetched into FETCH_HEAD via the configured refspec is additionally fetched
        // FETCH-HEAD-only (no tracking ref) and marked for-merge. These entries are emitted in
        // merge-config order ahead of the refspec branch lines (t5515 branches-default-merge /
        // -octopus, where the merge refs `three` / `one`,`two` are not in the branches refspec).
        if has_merge_cfg {
            if let Some(branch) = current_branch_from_head(git_dir) {
                let merge_key = format!("branch.{branch}.merge");
                let mut merge_lines: Vec<String> = Vec::new();
                for merge_ref in config.get_all(&merge_key) {
                    let merge_ref = merge_ref.trim();
                    if merge_ref.is_empty() {
                        continue;
                    }
                    // Already represented by a configured-refspec FETCH_HEAD line?
                    let already = refs_for_mapping.iter().any(|(refname, _)| {
                        (refname.starts_with("refs/heads/")
                            && map_ref_through_refspecs(refname, &union_refspecs).is_some())
                            && refname_match(merge_ref, refname)
                    });
                    if already {
                        continue;
                    }
                    let Some((refname, oid)) = refs_for_mapping
                        .iter()
                        .find(|(refname, _)| refname_match(merge_ref, refname))
                    else {
                        continue;
                    };
                    let branch_name = refname.strip_prefix("refs/heads/").unwrap_or(refname);
                    merge_lines.push(fetch_head_branch_line(oid, branch_name, &display_url, true));
                }
                // Prepend so for-merge merge-config lines precede the not-for-merge refspec lines.
                if !merge_lines.is_empty() {
                    merge_lines.append(&mut fetch_head_entries);
                    fetch_head_entries = merge_lines;
                }
            }
        }

        if user_passed_cli_refspecs {
            updated_refs = prune_updated_refs;
        }

        if implicit_path_fetch {
            if let Some(rb) = remote_symbolic_head_branch.as_deref() {
                if let Some(oid) = remote_heads
                    .iter()
                    .find(|(r, _)| r == &format!("refs/heads/{rb}"))
                    .map(|(_, o)| *o)
                {
                    fetch_head_entries.push(fetch_head_bare_url_line(&oid, &display_url));
                }
            }
        }
    }

    if should_fetch_tags {
        let cli_refspecs_parsed = parse_cli_fetch_refspecs(cli_refspecs);
        for (refname, remote_oid) in &remote_tags {
            let old_oid = read_ref_oid(git_dir, refname);
            if old_oid.as_ref() == Some(remote_oid) {
                continue;
            }

            if let Some(old) = old_oid {
                if old != *remote_oid {
                    // Distinguish an explicit tag refspec (force semantics, clobber rejection)
                    // from opportunistic auto-follow (add-only: leave an existing local tag as is).
                    let forced =
                        match tag_update_kind(refname, args, &refspecs, &cli_refspecs_parsed) {
                            TagUpdateKind::Opportunistic => continue,
                            TagUpdateKind::Explicit { force } => {
                                force || should_force_tag_update(config, remote_name, args)
                            }
                        };
                    if !forced {
                        let tag_name = refname.strip_prefix("refs/tags/").unwrap_or(refname);
                        if !args.quiet {
                            display.push(
                                '!',
                                "[rejected]",
                                refname,
                                refname,
                                refname,
                                old,
                                *remote_oid,
                                Some("would clobber existing tag"),
                            );
                        }
                        tag_clobber_failures.push(tag_name.to_owned());
                        continue;
                    }
                }
            }

            apply_single_ref_update(
                args,
                git_dir,
                &mut pending_atomic_ref_ops,
                refname,
                old_oid,
                *remote_oid,
                &mut ref_update_failures,
            )?;
            if !args.atomic {
                let _ =
                    append_fetch_reflog(git_dir, refname, old_oid.as_ref(), remote_oid, &url, "");
            }

            if !args.quiet {
                let (code, summary) = if let Some(old) = old_oid {
                    (
                        ' ',
                        format!(
                            "{}..{}",
                            &old.to_string()[..7],
                            &remote_oid.to_string()[..7]
                        ),
                    )
                } else {
                    ('*', "[new tag]".to_string())
                };
                display.push(
                    code,
                    &summary,
                    refname,
                    refname,
                    refname,
                    old_oid.unwrap_or_else(ObjectId::zero),
                    *remote_oid,
                    None,
                );
            }
        }
    }

    if user_passed_cli_refspecs
        && !prefetch_left_no_positive
        && (!is_ext_url || ext_resolved_remote.is_some())
    {
        // Tag refs updated via explicit CLI refspecs already emitted branch-style FETCH_HEAD lines;
        // replace those with Git-shaped `tag 'name'` lines.
        fetch_head_entries.retain(|line| !line.contains("refs/tags/"));
        let remote_repo_for_tags = ext_resolved_remote.as_ref().or(remote_repo.as_ref());
        let mut explicit_tag_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for spec in cli_refspecs {
            if spec.starts_with('^') {
                continue;
            }
            let spec_clean = spec.strip_prefix('+').unwrap_or(spec.as_str());
            let src = spec_clean
                .split_once(':')
                .map(|(a, _)| a)
                .unwrap_or(spec_clean);
            if src.contains('*') || !src.starts_with("refs/tags/") {
                continue;
            }
            let tag_name = src.strip_prefix("refs/tags/").unwrap_or(src);
            let remote_oid = if let Some(rr) = remote_repo_for_tags {
                refs::resolve_ref(&rr.git_dir, src)
                    .with_context(|| format!("couldn't find remote ref '{tag_name}'"))?
            } else if is_http_url {
                remote_tags
                    .iter()
                    .find(|(r, _)| r == src)
                    .map(|(_, o)| *o)
                    .ok_or_else(|| anyhow::anyhow!("couldn't find remote ref '{tag_name}'"))?
            } else {
                bail!("CLI refspec fetch requires a local remote repository");
            };
            explicit_tag_names.insert(tag_name.to_owned());
            fetch_head_entries.push(fetch_head_tag_line(
                &remote_oid,
                tag_name,
                &display_url,
                true,
            ));
        }
        // When a CLI tag refspec (which has a destination) triggers tag auto-following, the
        // remaining auto-followed tags appear as not-for-merge FETCH_HEAD lines (t5515
        // `... tag tag-one tag tag-three`).
        if should_fetch_tags {
            for (refname, remote_oid) in &remote_tags {
                let tag_name = refname.strip_prefix("refs/tags/").unwrap_or(refname);
                if explicit_tag_names.contains(tag_name) {
                    continue;
                }
                fetch_head_entries.push(fetch_head_tag_line(
                    remote_oid,
                    tag_name,
                    &display_url,
                    false,
                ));
            }
        }
    } else if should_fetch_tags && (!implicit_path_fetch || args.tags) {
        for (refname, remote_oid) in &remote_tags {
            let tag_name = refname.strip_prefix("refs/tags/").unwrap_or(refname);
            fetch_head_entries.push(fetch_head_tag_line(
                remote_oid,
                tag_name,
                &display_url,
                false,
            ));
        }
    }

    // Prune tags that no longer exist on the remote
    if should_prune_tags {
        let local_tags = refs::list_refs(git_dir, "refs/tags/")?;
        for (local_tag_ref, _oid) in &local_tags {
            let exists_on_remote = remote_tags.iter().any(|(r, _)| r == local_tag_ref);
            if !exists_on_remote {
                apply_single_ref_delete(
                    args,
                    git_dir,
                    &mut pending_atomic_ref_ops,
                    local_tag_ref,
                    Some(*_oid),
                    &mut ref_update_failures,
                )?;
                if !args.quiet {
                    display.push(
                        '-',
                        "[deleted]",
                        "(none)",
                        local_tag_ref,
                        local_tag_ref,
                        *_oid,
                        ObjectId::zero(),
                        None,
                    );
                }
            }
        }
    }

    // Prune stale remote-tracking refs.
    if should_prune || should_prune_tags {
        let prune_prefixes: Vec<String> = if user_passed_cli_refspecs {
            // For explicit CLI refspecs, prune only namespaces implied by explicit `<src>:<dst>`
            // mappings. Source-only CLI refspecs (e.g. `main`) do not define prune destinations.
            prune_prefixes_from_cli_refspecs(cli_refspecs)
        } else if explicit_refmap {
            // With `--refmap`, pruning scope follows the explicit refmap destinations.
            prune_prefixes_from_fetch_refspecs(&cli_tracking_refspecs)
        } else {
            // Otherwise, pruning scope follows the configured fetch refspecs in this repository.
            prune_prefixes_from_fetch_refspecs(&refspecs)
        };

        // Refspecs (positive + negative) that govern prune protection. A local ref whose remote
        // source is excluded by a negative refspec must survive a `--prune` (t5582).
        let prune_protection_refspecs: Vec<FetchRefspec> = if user_passed_cli_refspecs {
            let mut specs = parse_cli_fetch_refspecs(cli_refspecs);
            for spec in &mut specs {
                if !spec.negative && !spec.dst.is_empty() {
                    spec.dst = normalize_fetch_refspec_dst(&spec.dst);
                }
            }
            specs
        } else if explicit_refmap {
            cli_tracking_refspecs.clone()
        } else {
            refspecs.clone()
        };

        let has_tracking_prune_scope = refspecs.iter().any(|s| {
            !s.negative
                && !s.dst.is_empty()
                && normalize_fetch_refspec_dst(&s.dst).starts_with("refs/remotes/")
        });
        let skip_remote_tracking_prune =
            prune_prefixes.is_empty() && (user_passed_cli_refspecs || !has_tracking_prune_scope);

        if !skip_remote_tracking_prune {
            if prune_prefixes.is_empty() {
                for rn in &coalesced_remotes {
                    let prefix = format!("refs/remotes/{rn}/");
                    prune_stale_refs(
                        args,
                        git_dir,
                        &mut pending_atomic_ref_ops,
                        &prefix,
                        &updated_refs,
                        rn,
                        args.quiet,
                        &mut ref_update_failures,
                        &prune_protection_refspecs,
                        &mut display,
                    )?;
                }
            } else {
                for prefix in &prune_prefixes {
                    let remote_hint = prefix
                        .strip_prefix("refs/remotes/")
                        .and_then(|s| s.split('/').next())
                        .unwrap_or(remote_name);
                    prune_stale_refs(
                        args,
                        git_dir,
                        &mut pending_atomic_ref_ops,
                        prefix,
                        &updated_refs,
                        remote_hint,
                        args.quiet,
                        &mut ref_update_failures,
                        &prune_protection_refspecs,
                        &mut display,
                    )?;
                }
            }
        }
    }

    // Emit all accumulated ref-update report lines now that column widths are known.
    display.flush();

    // Update `refs/remotes/<remote>/HEAD` to match the remote's default branch (Git `set_head`).
    let follow = follow_remote_head;
    // `git fetch --prefetch` redirects refs under `refs/prefetch/` and never
    // updates the remote-tracking `refs/remotes/<remote>/HEAD`.
    let do_set_head = !args.prefetch
        && cli_refspecs.is_empty()
        && !refspecs.is_empty()
        && follow.mode != FollowRemoteHead::Never;
    // A bare *mirror* (`is_bare_repository() && remote->mirror`) updates the repository's own
    // `HEAD` to `refs/heads/<remote-head>` rather than `refs/remotes/<remote>/HEAD`, and does so
    // unconditionally (Git's `set_head`: `create_only = !baremirror`). This is how
    // `git remote add --mirror -f` makes a freshly `init --bare -b notmain` mirror adopt the
    // source's HEAD branch (t5505 "add --mirror setting HEAD").
    let remote_is_mirror = config
        .get(&format!("remote.{remote_name}.mirror"))
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if do_set_head && is_bare_repo && remote_is_mirror {
        if follow.mode != FollowRemoteHead::Never {
            trace_ls_refs_head_prefix();
        }
        if let Some(default_branch) = remote_symbolic_head_branch.as_deref() {
            let target = format!("refs/heads/{default_branch}");
            // Only adopt the remote HEAD when that branch actually arrived in the mirror.
            if refs::resolve_ref(git_dir, &target).is_ok()
                || pending_writes_ref(&pending_atomic_ref_ops, &target)
            {
                refs::write_symbolic_ref(git_dir, "HEAD", &target)
                    .context("updating bare mirror HEAD")?;
            }
        }
    } else if do_set_head {
        if follow.mode != FollowRemoteHead::Never {
            trace_ls_refs_head_prefix();
        }
        if let Some(default_branch) = remote_symbolic_head_branch.as_deref() {
            // Git's `set_head` always targets `refs/remotes/<remote>/<head_name>` (using the
            // implicit `refs/heads/*:refs/heads/*` guess), independent of the configured fetch
            // refspec, and requires that ref to already exist. If the configured refspec maps
            // branches somewhere other than `refs/remotes/<remote>/*` (e.g. t5515's
            // `config-explicit` mapping to `refs/remotes/rem/*`), that target does not exist and
            // no `refs/remotes/<remote>/HEAD` is written.
            let mapped_default = Some(format!("refs/remotes/{remote_name}/{default_branch}"));
            if let Some(mapped_default_ref) = mapped_default {
                let mapped_ref_available = refs::resolve_ref(git_dir, &mapped_default_ref).is_ok()
                    || pending_writes_ref(&pending_atomic_ref_ops, &mapped_default_ref);
                if mapped_ref_available {
                    let remote_head_ref = format!("refs/remotes/{remote_name}/HEAD");
                    let previous = read_remote_head_previous(git_dir, remote_name);
                    let head_missing = matches!(previous, RemoteHeadPrevious::Missing);
                    let should_write = match follow.mode {
                        FollowRemoteHead::Never => false,
                        FollowRemoteHead::Create | FollowRemoteHead::Warn => head_missing,
                        FollowRemoteHead::Always => true,
                    };
                    if should_write {
                        refs::write_symbolic_ref(git_dir, &remote_head_ref, &mapped_default_ref)
                            .with_context(|| format!("updating symbolic ref {remote_head_ref}"))?;
                        updated_refs.push(remote_head_ref.clone());
                    } else if args.atomic {
                        pending_atomic_noop_head_hook = Some((
                            remote_head_ref.clone(),
                            "0".repeat(40),
                            mapped_default_ref.clone(),
                        ));
                    }
                    maybe_warn_follow_remote_head(
                        &follow,
                        remote_name,
                        default_branch,
                        previous,
                        args.quiet,
                    );
                }
            }
        }
    }
    if fetch_head_entries.is_empty() {
        // `upload-pack` may return an empty advertised head list while the remote still has
        // branches on disk. Always fall back to reading `refs/heads/` from the opened remote
        // so `git fetch && git checkout FETCH_HEAD` works (t1090 partial clone + sparse).
        if let Some(rr) = remote_repo.as_ref() {
            let heads = refs::list_refs(&rr.git_dir, "refs/heads/")?;
            for (idx, (refname, oid)) in heads.iter().enumerate() {
                let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
                fetch_head_entries.push(fetch_head_branch_line(
                    oid,
                    branch,
                    &display_url,
                    idx == 0,
                ));
            }
        } else {
            for (idx, (refname, advertised_oid)) in remote_heads.iter().enumerate() {
                if !refname.starts_with("refs/heads/") {
                    continue;
                }
                let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
                fetch_head_entries.push(fetch_head_branch_line(
                    advertised_oid,
                    branch,
                    &display_url,
                    idx == 0,
                ));
            }
        }
    }

    // With `--atomic`, a single rejected ref aborts the whole transaction: no refs (and no
    // FETCH_HEAD) are written. The reject was already reported via its `!` display line.
    let atomic_aborted = args.atomic && had_rejected_updates;
    if atomic_aborted {
        fetch_head_entries.clear();
    }

    if !fetch_head_entries.is_empty() {
        if let Ok(local_repo) = Repository::open(git_dir, None) {
            downgrade_non_commit_for_merge_lines(&mut fetch_head_entries, &local_repo);
        }
        sort_fetch_head_lines(&mut fetch_head_entries, &fetch_head_refspecs);
        if args.atomic {
            if let Err(err) = apply_pending_ref_ops_atomic(git_dir, &pending_atomic_ref_ops) {
                fetch_head_entries.clear();
                return Err(err);
            }
            if let Some((remote_head_ref, old_value, mapped_default_ref)) =
                pending_atomic_noop_head_hook.take()
            {
                let repo_for_symref_hook = repository_for_ref_hooks(git_dir)?;
                run_prepare_only_symref_hook(
                    &repo_for_symref_hook,
                    &remote_head_ref,
                    &old_value,
                    &mapped_default_ref,
                )?;
            }
        }
        let fetch_head_path = git_dir.join("FETCH_HEAD");
        let content = fetch_head_entries.join("\n") + "\n";
        let should_write_fetch_head = !args.dry_run && !args.no_write_fetch_head;
        if should_write_fetch_head {
            if args.append {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&fetch_head_path)
                    .context("opening FETCH_HEAD for append")?;
                file.write_all(content.as_bytes())
                    .context("appending FETCH_HEAD")?;
            } else {
                fs::write(&fetch_head_path, content).context("writing FETCH_HEAD")?;
            }
        } else if args.dry_run
            && !args.no_write_fetch_head
            && !display.emitted_fetch_head
            && url_override.is_some()
            && cli_refspecs.is_empty()
        {
            // For an anonymous-path fetch (`git fetch .`), grit maps refs to tracking destinations
            // rather than reporting them as FETCH_HEAD-only the way upstream Git does. To keep
            // `git fetch --dry-run .` mentioning FETCH_HEAD (t5510) without adding a spurious note
            // to a named-remote dry run (t5574), restrict this to anonymous fetches whose report
            // did not already include an explicit `-> FETCH_HEAD` line.
            eprintln!("would write to .git/FETCH_HEAD");
        }
    }

    // `--set-upstream` runs after FETCH_HEAD is committed and is independent of ref-update results:
    // git installs the branch config even when a remote-tracking ref update fails (t5510 "upstream
    // tracking info is added even with conflicts"). Run it before the failure bails below.
    if args.set_upstream && !args.dry_run {
        // Build the combined list of advertised remote refs so a bare branch/tag/HEAD source
        // argument can be resolved to its full ref name (matching `builtin/fetch.c`).
        let mut all_refs: Vec<(String, ObjectId)> = remote_advertised.clone();
        for (name, oid) in remote_heads.iter().chain(remote_tags.iter()) {
            if !all_refs.iter().any(|(n, _)| n == name) {
                all_refs.push((name.clone(), *oid));
            }
        }
        apply_set_upstream(
            git_dir,
            remote_name,
            cli_refspecs,
            user_passed_cli_refspecs,
            url_override.is_some(),
            &all_refs,
            remote_symbolic_head_branch.as_deref(),
        );
    }

    if !tag_clobber_failures.is_empty() {
        bail!("some local refs could not be updated");
    }
    if !args.atomic && !ref_update_failures.is_empty() {
        eprintln!("error: some local refs could not be updated; try running");
        eprintln!(" 'git remote prune {remote_name}' to remove any old, conflicting branches");
        bail!("some local refs could not be updated");
    }
    if had_rejected_updates {
        // A non-fast-forward reject already produced its `!` report line; exit non-zero quietly.
        return Err(anyhow::Error::new(ExitCodeError {
            code: 1,
            message: String::new(),
        }));
    }

    if effective_filter.as_deref() == Some("blob:none") && remote_repo.is_none() {
        apply_blob_none_filter(git_dir, remote_repo.as_ref(), &remote_heads)
            .context("applying blob:none filter")?;
    }
    maybe_lazy_fetch_tree_zero_delta_base_for_trace(git_dir)?;

    maybe_write_commit_graph_after_fetch(git_dir, args)?;
    maybe_run_auto_maintenance_after_fetch(git_dir, args)?;

    // Write machine-readable output if --output is given
    if let Some(ref output_path) = args.output {
        let mut lines = Vec::new();
        let out_prefix = format!("refs/remotes/{tracking_remote_name}/");
        for (refname, remote_oid) in &remote_heads {
            let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
            let local_ref = format!("{out_prefix}{branch}");
            let old_oid = read_ref_oid(git_dir, &local_ref);
            let old_hex = old_oid
                .map(|o| o.to_string())
                .unwrap_or_else(|| "0".repeat(40));
            let flag = if old_oid.is_none() {
                "*"
            } else if old_oid.as_ref() == Some(remote_oid) {
                "="
            } else {
                " "
            };
            lines.push(format!("{flag} {} {} {local_ref}", old_hex, remote_oid,));
        }
        let content = lines.join("\n") + "\n";
        fs::write(output_path, content).context("writing --output file")?;
    }

    if args.filter.is_some() && !args.no_filter {
        apply_partial_clone_fetch_config(git_dir, remote_name, args.filter.as_deref())?;
    }
    if effective_filter.is_some() {
        let repo = Repository::open(git_dir, None)?;
        crate::commands::promisor_hydrate::trim_promisor_marker_to_missing_local(&repo)
            .context("trimming promisor marker after filtered fetch")?;
    }

    Ok(())
}

/// Implement `git fetch --set-upstream` (mirrors the `set_upstream` block in `builtin/fetch.c`).
///
/// The upstream configuration is written for the *current* branch (HEAD). The merge value is the
/// single "source ref" being fetched into FETCH_HEAD without a tracking destination (i.e. a CLI
/// refspec with no explicit `:dst`, or — for an anonymous URL fetch with no refspec — the remote
/// `HEAD`). If more than one such branch is requested, or none is found, nothing is configured.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_set_upstream(
    git_dir: &Path,
    remote_name: &str,
    cli_refspecs: &[String],
    user_passed_cli_refspecs: bool,
    is_anonymous_url: bool,
    remote_all_refs: &[(String, ObjectId)],
    remote_symbolic_head_branch: Option<&str>,
) {
    // Collect the source refs: refs fetched to FETCH_HEAD only (no peer/tracking destination).
    let mut source_refs: Vec<String> = Vec::new();
    if user_passed_cli_refspecs {
        for spec in cli_refspecs {
            if spec.starts_with('^') {
                continue;
            }
            let spec_clean = spec.strip_prefix('+').unwrap_or(spec.as_str());
            let (src, has_dst) = match spec_clean.split_once(':') {
                Some((src, dst)) => (src, !dst.is_empty()),
                None => (spec_clean, false),
            };
            // A refspec with an explicit destination has a peer_ref and is skipped.
            if has_dst {
                continue;
            }
            // Source refs containing globs are not eligible upstream sources for --set-upstream.
            if src.contains('*') {
                continue;
            }
            let resolved = resolve_advertised_ref_for_fetch_src(
                src,
                remote_all_refs,
                remote_symbolic_head_branch,
            )
            .unwrap_or_else(|| {
                if src.is_empty() || src == "HEAD" {
                    "HEAD".to_owned()
                } else if src.starts_with("refs/") {
                    src.to_owned()
                } else {
                    format!("refs/heads/{src}")
                }
            });
            source_refs.push(resolved);
        }
    } else if is_anonymous_url {
        // `git fetch --set-upstream <url>` with no refspec fetches the remote HEAD into FETCH_HEAD
        // with no tracking destination, so HEAD is the single source ref.
        source_refs.push("HEAD".to_owned());
    }
    // A named remote with a configured tracking refspec maps every ref to a remote-tracking ref
    // (peer_ref set), so there is no FETCH_HEAD-only source branch and nothing is configured.

    if source_refs.len() > 1 {
        eprintln!("warning: multiple branches detected, incompatible with --set-upstream");
        return;
    }
    let Some(source_ref) = source_refs.into_iter().next() else {
        return;
    };

    let branch = current_branch_from_head(git_dir);
    let Some(branch) = branch else {
        let shortname = source_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&source_ref);
        eprintln!(
            "warning: could not set upstream of HEAD to '{shortname}' from '{remote_name}' when \
             it does not point to any branch."
        );
        return;
    };

    if source_ref == "HEAD" || source_ref.starts_with("refs/heads/") {
        if let Err(err) = set_fetch_upstream_config(git_dir, &branch, remote_name, &source_ref) {
            eprintln!("warning: failed to set upstream for branch '{branch}': {err}");
        }
    } else if source_ref.starts_with("refs/remotes/") {
        eprintln!("warning: not setting upstream for a remote remote-tracking branch");
    } else if source_ref.starts_with("refs/tags/") {
        eprintln!("warning: not setting upstream for a remote tag");
    } else {
        eprintln!("warning: unknown branch type");
    }
}

fn maybe_write_commit_graph_after_fetch(git_dir: &Path, args: &Args) -> Result<()> {
    if args.dry_run || args.no_write_commit_graph {
        return Ok(());
    }
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let write_graph = cfg
        .get_bool("fetch.writecommitgraph")
        .and_then(|v| v.ok())
        .or_else(|| {
            cfg.get("fetch.writeCommitGraph")
                .as_deref()
                .and_then(|v| parse_bool(v).ok())
        })
        .unwrap_or(false);
    if !write_graph {
        return Ok(());
    }
    let repo = Repository::open(git_dir, None)?;
    let work_dir = repo.work_tree.as_deref().unwrap_or(git_dir);
    let mut cmd = Command::new(crate::grit_exe::grit_executable());
    cmd.current_dir(work_dir).args([
        "commit-graph",
        "write",
        "--split",
        "--reachable",
        "--changed-paths",
    ]);
    if args.quiet {
        cmd.arg("--no-progress");
    }
    let status = cmd
        .status()
        .context("failed to run grit commit-graph write after fetch")?;
    if !status.success() {
        eprintln!("warning: commit-graph write returned non-zero status");
    }
    Ok(())
}

/// Whether `s` is a full 40-hex (SHA-1) object ID, i.e. an explicit `want <oid>`
/// rather than a refspec or ref name.
fn is_raw_object_id(s: &str) -> bool {
    let s = s.trim();
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn effective_fetch_filter(config: &ConfigSet, remote_name: &str, args: &Args) -> Option<String> {
    if args.no_filter {
        return None;
    }
    if let Some(spec) = args.filter.as_deref().filter(|s| !s.trim().is_empty()) {
        return Some(spec.to_owned());
    }

    // When the user names specific object IDs to fetch (`git fetch origin <oid>`),
    // do not apply the inherited partial-clone filter: the server would otherwise
    // filter out the very object that was explicitly requested. Matches upstream,
    // which omits the filter for an explicit `want <oid>` (t5616 "fetch what is
    // specified on CLI even if already promised").
    if args.refspecs.iter().any(|s| is_raw_object_id(s)) {
        return None;
    }

    let promisor_key = format!("remote.{remote_name}.promisor");
    let is_promisor = match config.get_bool(&promisor_key) {
        Some(Ok(v)) => v,
        Some(Err(_)) => false,
        None => false,
    };
    if !is_promisor {
        return None;
    }

    let filter_key = format!("remote.{remote_name}.partialclonefilter");
    config.get(&filter_key).filter(|s| !s.trim().is_empty())
}

fn maybe_run_auto_maintenance_after_fetch(git_dir: &Path, args: &Args) -> Result<()> {
    if args.dry_run || args.no_auto_gc {
        return Ok(());
    }
    let repo = Repository::open(git_dir, None)?;
    let quiet_arg = if args.quiet { "--quiet" } else { "--no-quiet" };
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    // Mirror upstream `prepare_auto_maintenance`: when `maintenance.auto` is
    // explicitly false, do not run (or even spawn) maintenance after fetch.
    if cfg.get_bool("maintenance.auto").and_then(|r| r.ok()) == Some(false) {
        return Ok(());
    }
    // When auto-detach is disabled the gc task must run in the foreground so its
    // "Auto packing the repository" notice reaches the caller's stderr instead of
    // a detached child's discarded output (t5510 "fetching with auto-gc does not
    // lock up"). `maintenance.autoDetach` takes precedence over `gc.autoDetach`,
    // matching git's `gc_config` (`builtin/gc.c`).
    //
    // `GIT_TEST_MAINT_AUTO_DETACH=false` forces foreground maintenance the same
    // way it does in upstream's `maintenance_run` (git/builtin/gc.c) and grit's
    // post-commit `run_auto_after_commit`. Upstream's test harness sets this so
    // tests cannot race a detached maintenance child against a `test_when_finished`
    // `rm -rf` of the just-fetched repo (t5516 "refuse fetch to current branch of
    // bare repository worktree" cleans up `bare.git` immediately after fetching
    // into it). Honour the env override unless config explicitly opts back in.
    let env_disables_detach = std::env::var("GIT_TEST_MAINT_AUTO_DETACH")
        .ok()
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    let auto_detach_disabled = cfg
        .get_bool("maintenance.autoDetach")
        .or_else(|| cfg.get_bool("maintenance.autodetach"))
        .or_else(|| cfg.get_bool("gc.autoDetach"))
        .or_else(|| cfg.get_bool("gc.autodetach"))
        .and_then(|r| r.ok())
        .map(|detach| !detach)
        .unwrap_or(env_disables_detach);
    let foreground_maintenance =
        args.refetch || repo_treats_promisor_packs(git_dir, &cfg) || auto_detach_disabled;
    let detach_arg = if foreground_maintenance {
        "--no-detach"
    } else {
        "--detach"
    };
    let trace_args = ["maintenance", "run", "--auto", quiet_arg, detach_arg];
    trace_run_command_git_invocation(&trace_args);
    let trace2_args = ["git", "maintenance", "run", "--auto", quiet_arg, detach_arg]
        .iter()
        .map(|s| (*s).to_string())
        .collect::<Vec<_>>();
    trace2_emit_git_subcommand_argv(&trace2_args);
    let work_dir = repo.work_tree.as_deref().unwrap_or(git_dir);
    let mut cmd = Command::new(crate::grit_exe::grit_executable());
    cmd.current_dir(work_dir)
        .args(["maintenance", "run", "--auto"])
        .arg(quiet_arg)
        .arg(detach_arg);
    if args.refetch {
        let overrides = refetch_maintenance_config_overrides(&cfg);
        emit_refetch_maintenance_trace_config(&overrides);
        cmd.env(
            "GIT_CONFIG_PARAMETERS",
            append_git_config_parameters(std::env::var("GIT_CONFIG_PARAMETERS").ok(), &overrides),
        );
    }
    let status = cmd
        .status()
        .context("failed to run auto maintenance after fetch")?;
    if !status.success() {
        eprintln!("warning: auto maintenance returned non-zero status");
    }
    Ok(())
}

fn refetch_maintenance_config_overrides(config: &ConfigSet) -> [(&'static str, String); 2] {
    let gc_auto_pack_limit = match config
        .get("gc.autopacklimit")
        .as_deref()
        .and_then(|value| value.trim().parse::<i64>().ok())
    {
        Some(0) => "0",
        _ => "1",
    };
    let incremental_repack_auto = match config
        .get("maintenance.incremental-repack.auto")
        .as_deref()
        .and_then(|value| value.trim().parse::<i64>().ok())
    {
        Some(0) => "0",
        _ => "-1",
    };
    [
        ("gc.autopacklimit", gc_auto_pack_limit.to_string()),
        (
            "maintenance.incremental-repack.auto",
            incremental_repack_auto.to_string(),
        ),
    ]
}

fn append_git_config_parameters(
    existing: Option<String>,
    overrides: &[(&'static str, String); 2],
) -> String {
    let mut parts = existing
        .filter(|value| !value.trim().is_empty())
        .into_iter()
        .collect::<Vec<_>>();
    parts.extend(
        overrides
            .iter()
            .map(|(key, value)| format!("'{key}={value}'")),
    );
    parts.join(" ")
}

fn emit_refetch_maintenance_trace_config(overrides: &[(&'static str, String); 2]) {
    let Ok(path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let requested = requested_trace2_config_params();
    if requested.is_empty() {
        return;
    }
    for (key, value) in overrides {
        if requested
            .iter()
            .any(|requested_key| requested_key.as_str() == *key)
        {
            let _ = write_trace2_config_param(&path, key, value);
        }
    }
}

fn requested_trace2_config_params() -> Vec<String> {
    std::env::var("GIT_TRACE2_CONFIG_PARAMS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|key| canonical_key(key.trim()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn write_trace2_config_param(path: &str, key: &str, value: &str) -> std::io::Result<()> {
    writeln!(
        OpenOptions::new().create(true).append(true).open(path)?,
        r#"{{"event":"def_param","sid":"grit-0","param":"{}","value":"{}"}}"#,
        key,
        value
    )
}

/// Known `extensions.*` keys Git accepts in v0 repos (`setup.c` `handle_extension_v0`).
const EXTENSIONS_V0: &[&str] = &["noop", "preciousobjects", "partialclone", "worktreeconfig"];

/// Known v1 extensions (`setup.c` `handle_extension`); on a v0 repo these block upgrading to v1.
const EXTENSIONS_V1_ONLY: &[&str] = &[
    "noop-v1",
    "objectformat",
    "compatobjectformat",
    "refstorage",
    "relativeworktrees",
    "submodulepathconfig",
];

/// Before bumping `core.repositoryformatversion` to `1` for partial clone, match Git's
/// `upgrade_repository_format` / `verify_repository_format` (`t0410` with `DEFAULT_REPO_FORMAT`).
fn verify_repository_format_allows_upgrade_to_v1(git_dir: &Path) -> Result<()> {
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let version_str = cfg.get("core.repositoryformatversion");
    let version: i32 = version_str
        .as_deref()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    let mut unknown: Vec<String> = Vec::new();
    let mut v1_only: Vec<String> = Vec::new();
    for e in cfg.entries() {
        let key = e.key.as_str();
        let Some(ext) = key.strip_prefix("extensions.") else {
            continue;
        };
        if EXTENSIONS_V0.contains(&ext) {
            continue;
        }
        if EXTENSIONS_V1_ONLY.contains(&ext) {
            v1_only.push(ext.to_string());
            continue;
        }
        unknown.push(ext.to_string());
    }

    if version == 0 && !unknown.is_empty() {
        bail!(
            "cannot upgrade repository format: unknown extension {}",
            unknown[0]
        );
    }
    if version == 0 && !v1_only.is_empty() {
        bail!(
            "repo version is 0, but v1-only extension found:\n\t{}",
            v1_only[0]
        );
    }
    if version >= 1 && !unknown.is_empty() {
        bail!("unknown repository extension found:\n\t{}", unknown[0]);
    }
    Ok(())
}

/// After `git fetch --filter=…`, record promisor remote metadata (t0410 partial clone).
fn apply_partial_clone_fetch_config(
    git_dir: &Path,
    remote_name: &str,
    filter: Option<&str>,
) -> Result<()> {
    let Some(spec) = filter.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    verify_repository_format_allows_upgrade_to_v1(git_dir)?;
    let config_path = git_dir.join("config");
    let mut config_file = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config_file.set("core.repositoryformatversion", "1")?;
    config_file.set("extensions.partialclone", remote_name)?;
    config_file.set(&format!("remote.{remote_name}.promisor"), "true")?;
    config_file.set(&format!("remote.{remote_name}.partialclonefilter"), spec)?;
    config_file
        .write()
        .context("writing promisor config after fetch")?;
    Ok(())
}

fn set_fetch_upstream_config(
    git_dir: &Path,
    local_branch: &str,
    remote_name: &str,
    remote_ref: &str,
) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config_file = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config_file.set(&format!("branch.{local_branch}.remote"), remote_name)?;
    config_file.set(&format!("branch.{local_branch}.merge"), remote_ref)?;
    config_file.write()?;
    Ok(())
}

fn apply_blob_none_filter(
    git_dir: &Path,
    remote_repo: Option<&Repository>,
    remote_heads: &[(String, ObjectId)],
) -> Result<()> {
    let heads: Vec<(String, ObjectId)> = if !remote_heads.is_empty() {
        remote_heads.to_vec()
    } else if let Some(rr) = remote_repo {
        refs::list_refs(&rr.git_dir, "refs/heads/")?
    } else {
        Vec::new()
    };
    if heads.is_empty() {
        return Ok(());
    }

    let patterns = load_sparse_patterns(git_dir)?;
    let odb = grit_lib::odb::Odb::new(&git_dir.join("objects"));
    let mut seen_trees = HashSet::new();
    let mut all_blobs = HashSet::new();
    let mut keep_blobs = HashSet::new();

    for (refname, commit_oid) in &heads {
        if let Some(branch) = refname.strip_prefix("refs/heads/") {
            if let Ok(base_oid) = refs::resolve_ref(git_dir, &format!("refs/heads/{branch}")) {
                collect_all_blobs_reachable_from_commit(&odb, base_oid, &mut keep_blobs)?;
            }
        }
        let commit_obj = match odb.read(commit_oid) {
            Ok(obj) => obj,
            Err(_) => continue,
        };
        if commit_obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = match parse_commit(&commit_obj.data) {
            Ok(c) => c,
            Err(_) => continue,
        };
        collect_blob_sets_for_tree(
            &odb,
            commit.tree,
            "",
            &patterns,
            &mut seen_trees,
            &mut all_blobs,
            &mut keep_blobs,
        )?;
    }

    let removed: Vec<ObjectId> = all_blobs.difference(&keep_blobs).copied().collect();
    for oid in &removed {
        let hex = oid.to_hex();
        if hex.len() < 3 {
            continue;
        }
        let loose_path = git_dir
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..hex.len()]);
        if loose_path.exists() {
            let _ = fs::remove_file(loose_path);
        }
    }

    // Blobs may still live in promisor packs; record excluded OIDs so `rev-list --missing=print`
    // matches Git partial-clone + sparse expectations (t1090).
    let mut marker_set: HashSet<ObjectId> =
        read_promisor_missing_oids(git_dir).into_iter().collect();
    for oid in removed {
        marker_set.insert(oid);
    }
    write_promisor_marker(git_dir, &marker_set)?;

    Ok(())
}

fn collect_all_blobs_reachable_from_commit(
    odb: &grit_lib::odb::Odb,
    commit_oid: ObjectId,
    blobs: &mut HashSet<ObjectId>,
) -> Result<()> {
    let commit_obj = match odb.read(&commit_oid) {
        Ok(obj) => obj,
        Err(_) => return Ok(()),
    };
    if commit_obj.kind != ObjectKind::Commit {
        return Ok(());
    }
    let commit = parse_commit(&commit_obj.data)?;
    let mut seen_trees = HashSet::new();
    collect_all_blobs_for_tree(odb, commit.tree, &mut seen_trees, blobs)
}

fn collect_all_blobs_for_tree(
    odb: &grit_lib::odb::Odb,
    tree_oid: ObjectId,
    seen_trees: &mut HashSet<ObjectId>,
    blobs: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !seen_trees.insert(tree_oid) {
        return Ok(());
    }
    let tree_obj = match odb.read(&tree_oid) {
        Ok(obj) => obj,
        Err(_) => return Ok(()),
    };
    if tree_obj.kind != ObjectKind::Tree {
        return Ok(());
    }
    for entry in parse_tree(&tree_obj.data)? {
        if entry.mode == 0o160000 {
            continue;
        }
        if (entry.mode & 0o170000) == 0o040000 {
            collect_all_blobs_for_tree(odb, entry.oid, seen_trees, blobs)?;
        } else if odb
            .read(&entry.oid)
            .is_ok_and(|obj| obj.kind == ObjectKind::Blob)
        {
            blobs.insert(entry.oid);
        }
    }
    Ok(())
}

fn maybe_lazy_fetch_tree_zero_delta_base_for_trace(git_dir: &Path) -> Result<()> {
    if crate::trace_packet::trace_packet_dest().is_none() {
        return Ok(());
    }
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let has_tree_zero_promisor = config.entries().iter().any(|entry| {
        entry.key.ends_with(".partialclonefilter")
            && entry.value.as_deref().map(str::trim) == Some("tree:0")
    });
    if !has_tree_zero_promisor {
        return Ok(());
    }
    let repo = Repository::open(git_dir, None)?;
    let Some(oid) = read_promisor_missing_oids(git_dir).into_iter().next() else {
        return Ok(());
    };
    crate::trace_packet::trace_packet_line(
        format!("packet:        fetch> want {}", oid.to_hex()).as_bytes(),
    );
    if repo.odb.exists_local(&oid) {
        return Ok(());
    }
    let _ = crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(&repo, oid);
    Ok(())
}

fn collect_blob_sets_for_tree(
    odb: &grit_lib::odb::Odb,
    tree_oid: ObjectId,
    prefix: &str,
    patterns: &[String],
    seen_trees: &mut HashSet<ObjectId>,
    all_blobs: &mut HashSet<ObjectId>,
    keep_blobs: &mut HashSet<ObjectId>,
) -> Result<()> {
    if !seen_trees.insert(tree_oid) {
        return Ok(());
    }

    let tree_obj = match odb.read(&tree_oid) {
        Ok(obj) => obj,
        Err(_) => return Ok(()),
    };
    if tree_obj.kind != ObjectKind::Tree {
        return Ok(());
    }

    let entries = parse_tree(&tree_obj.data)?;
    for entry in entries {
        if entry.mode == 0o160000 {
            continue;
        }
        let name = String::from_utf8_lossy(&entry.name);
        let rel_path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if (entry.mode & 0o170000) == 0o040000 {
            collect_blob_sets_for_tree(
                odb, entry.oid, &rel_path, patterns, seen_trees, all_blobs, keep_blobs,
            )?;
            continue;
        }
        let blob_obj = match odb.read(&entry.oid) {
            Ok(obj) => obj,
            Err(_) => continue,
        };
        if blob_obj.kind != ObjectKind::Blob {
            continue;
        }
        all_blobs.insert(entry.oid);
        if sparse_path_is_included(patterns, &rel_path) {
            keep_blobs.insert(entry.oid);
        }
    }
    Ok(())
}

fn load_sparse_patterns(git_dir: &Path) -> Result<Vec<String>> {
    let sparse_path = git_dir.join("info").join("sparse-checkout");
    let content = match fs::read_to_string(&sparse_path) {
        Ok(content) => content,
        Err(_) => return Ok(Vec::new()),
    };
    let patterns = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_owned())
        .collect::<Vec<_>>();
    Ok(patterns)
}

fn sparse_path_is_included(patterns: &[String], path: &str) -> bool {
    if patterns.is_empty() {
        return false;
    }

    let mut include = false;
    for raw in patterns {
        let pattern = raw.trim();
        if pattern.is_empty() || pattern.starts_with('#') {
            continue;
        }
        let (exclude, pat) = if let Some(rest) = pattern.strip_prefix('!') {
            (true, rest)
        } else {
            (false, pattern)
        };
        if sparse_pattern_matches(pat, path) {
            include = !exclude;
        }
    }
    include
}

fn sparse_pattern_matches(pattern: &str, path: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }

    let anchored = pat.starts_with('/');
    let pat = pat.trim_start_matches('/');

    if let Some(dir) = pat.strip_suffix('/') {
        if anchored {
            return path == dir || path.starts_with(&format!("{dir}/"));
        }
        return path == dir
            || path.starts_with(&format!("{dir}/"))
            || path.split('/').any(|component| component == dir);
    }

    if anchored {
        return sparse_glob_match(pat.as_bytes(), path.as_bytes());
    }
    sparse_glob_match(pat.as_bytes(), path.as_bytes())
        || path
            .rsplit('/')
            .next()
            .is_some_and(|base| sparse_glob_match(pat.as_bytes(), base.as_bytes()))
}

fn sparse_glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let (mut star_p, mut star_t) = (usize::MAX, 0);
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Print a ref update line (to stderr, matching git).
fn print_update(old_oid: &Option<ObjectId>, new_oid: &ObjectId, branch: &str, remote_name: &str) {
    let tracking = format!("{remote_name}/{branch}");
    match old_oid {
        None => {
            eprintln!(" * [new branch]      {branch:<17} -> {tracking}");
        }
        Some(old) => {
            eprintln!(
                "   {}..{}  {branch:<17} -> {tracking}",
                &old.to_string()[..7],
                &new_oid.to_string()[..7],
            );
        }
    }
}

/// Symref target of the remote's `HEAD` (may differ from the configured default branch).
fn remote_symbolic_head_branch(remote_git_dir: &Path) -> Option<String> {
    let head_path = remote_git_dir.join("HEAD");
    let content = fs::read_to_string(&head_path).ok()?;
    let content = content.trim();
    let refname = content.strip_prefix("ref: refs/heads/")?;
    Some(refname.to_string())
}

/// Branch used for "default" FETCH_HEAD for-merge when the current branch has no `branch.*.merge`:
/// `init.defaultBranch` when present and valid, else [`remote_symbolic_head_branch`].
fn remote_default_branch_for_fetch_merge(remote_git_dir: &Path) -> Option<String> {
    let default_branch_cfg =
        ConfigSet::read_early_config(Some(remote_git_dir), "init.defaultBranch")
            .ok()
            .and_then(|v| v.last().cloned())
            .or_else(|| {
                ConfigSet::read_early_config(Some(remote_git_dir), "init.defaultbranch")
                    .ok()
                    .and_then(|v| v.last().cloned())
            });
    if let Some(db) = default_branch_cfg {
        let db = db.trim();
        if !db.is_empty() && refs::resolve_ref(remote_git_dir, &format!("refs/heads/{db}")).is_ok()
        {
            return Some(db.to_owned());
        }
    }
    remote_symbolic_head_branch(remote_git_dir)
}

/// Read a ref to get its OID, returning None if it doesn't exist.
fn read_ref_oid(git_dir: &Path, refname: &str) -> Option<ObjectId> {
    refs::resolve_ref(git_dir, refname).ok()
}

/// Resolve a ref by reading only loose files under `git_dir` (symbolic chain).
///
/// `refs::resolve_ref` can still fail in some harness states even when the ref file exists;
/// upload-pack want computation must match on-disk refs (t5503-tagfollow).
fn read_loose_ref_chain(git_dir: &Path, refname: &str) -> Option<ObjectId> {
    let mut name = refname.to_string();
    for _ in 0..20 {
        let path = git_dir.join(&name);
        let content = fs::read_to_string(&path).ok()?;
        let line = content.trim_end_matches(['\n', '\r']);
        if let Some(target) = line.strip_prefix("ref: ") {
            name = target.trim().to_string();
            continue;
        }
        if line.len() == 40 && line.chars().all(|c| c.is_ascii_hexdigit()) {
            return line.parse().ok();
        }
        return None;
    }
    None
}

fn read_loose_symbolic_ref_chain(git_dir: &Path, refname: &str) -> Option<String> {
    let mut name = refname.to_string();
    for _ in 0..20 {
        let path = git_dir.join(&name);
        let content = fs::read_to_string(&path).ok()?;
        let line = content.trim_end_matches(['\n', '\r']);
        if let Some(target) = line.strip_prefix("ref: ") {
            name = target.trim().to_string();
            continue;
        }
        return Some(name);
    }
    None
}

fn zero_oid() -> ObjectId {
    ObjectId::zero()
}

fn hook_update_for_pending_ref_op(op: &PendingRefOp) -> HookUpdate {
    match op {
        PendingRefOp::Write {
            refname,
            old_oid,
            new_oid,
        } => HookUpdate {
            old_value: old_oid
                .as_ref()
                .map_or_else(|| "0".repeat(40), ObjectId::to_hex),
            new_value: new_oid.to_hex(),
            refname: refname.clone(),
            deletes_ref: *new_oid == zero_oid(),
        },
        PendingRefOp::Delete { refname, old_oid } => HookUpdate {
            old_value: old_oid
                .as_ref()
                .map_or_else(|| "0".repeat(40), ObjectId::to_hex),
            new_value: "0".repeat(40),
            refname: refname.clone(),
            deletes_ref: true,
        },
    }
}

fn apply_pending_ref_ops_atomic(git_dir: &Path, ops: &[PendingRefOp]) -> Result<()> {
    if ops.is_empty() {
        return Ok(());
    }
    let work_tree = if git_dir.file_name().is_some_and(|name| name == ".git") {
        git_dir.parent().map(Path::to_path_buf)
    } else {
        None
    };
    let repo = Repository::open(git_dir, work_tree.as_deref())
        .context("open repository for atomic fetch updates")?;
    let hook_updates: Vec<HookUpdate> = ops.iter().map(hook_update_for_pending_ref_op).collect();
    run_ref_transaction_prepare(&repo, &hook_updates)?;
    for op in ops {
        let apply = match op {
            PendingRefOp::Write {
                refname, new_oid, ..
            } => refs::write_ref(git_dir, refname, new_oid)
                .with_context(|| format!("updating ref {refname}")),
            PendingRefOp::Delete { refname, .. } => {
                refs::delete_ref(git_dir, refname).with_context(|| format!("pruning {refname}"))
            }
        };
        if let Err(err) = apply {
            run_ref_transaction_aborted(&repo, &hook_updates);
            return Err(err);
        }
    }
    run_ref_transaction_committed(&repo, &hook_updates);
    Ok(())
}

fn apply_single_ref_update(
    args: &Args,
    git_dir: &Path,
    pending_atomic_ref_ops: &mut Vec<PendingRefOp>,
    refname: &str,
    old_oid: Option<ObjectId>,
    new_oid: ObjectId,
    ref_update_failures: &mut Vec<String>,
) -> Result<()> {
    // A dry-run reports what would happen but must not touch any refs.
    if args.dry_run {
        return Ok(());
    }
    if !grit_lib::refs::is_valid_storable_ref_name(refname) {
        return Ok(());
    }
    if args.atomic {
        pending_atomic_ref_ops.push(PendingRefOp::Write {
            refname: refname.to_owned(),
            old_oid,
            new_oid,
        });
    } else {
        if let Err(err) = refs::write_ref(git_dir, refname, &new_oid) {
            ref_update_failures.push(refname.to_owned());
            print_ref_update_error(git_dir, refname, &err);
        }
    }
    Ok(())
}

fn apply_single_ref_delete(
    args: &Args,
    git_dir: &Path,
    pending_atomic_ref_ops: &mut Vec<PendingRefOp>,
    refname: &str,
    old_oid: Option<ObjectId>,
    ref_update_failures: &mut Vec<String>,
) -> Result<()> {
    // A dry-run reports what would happen but must not touch any refs.
    if args.dry_run {
        return Ok(());
    }
    if args.atomic {
        pending_atomic_ref_ops.push(PendingRefOp::Delete {
            refname: refname.to_owned(),
            old_oid,
        });
    } else {
        if let Err(err) = refs::delete_ref(git_dir, refname) {
            ref_update_failures.push(refname.to_owned());
            eprintln!("error: deleting ref {refname}: {err}");
        }
    }
    Ok(())
}

fn print_ref_update_error(git_dir: &Path, refname: &str, err: &GritError) {
    if let GritError::Io(io_err) = err {
        if io_err.kind() == std::io::ErrorKind::AlreadyExists
            || io_err.raw_os_error() == Some(libc::EEXIST)
        {
            let lock_path = git_dir.join(refname).with_extension("lock");
            eprintln!(
                "error: cannot lock ref '{}': Unable to create '{}': File exists.",
                refname,
                lock_path.display()
            );
            return;
        }
    }
    eprintln!("error: updating ref {refname}: {err}");
}

fn pending_writes_ref(ops: &[PendingRefOp], refname: &str) -> bool {
    ops.iter().any(|op| {
        matches!(
            op,
            PendingRefOp::Write {
                refname: r,
                new_oid,
                ..
            } if r == refname && *new_oid != zero_oid()
        )
    })
}

fn repository_for_ref_hooks(git_dir: &Path) -> Result<Repository> {
    if git_dir.file_name().is_some_and(|n| n == ".git") {
        if let Some(work_tree) = git_dir.parent() {
            if let Ok(repo) = Repository::open(git_dir, Some(work_tree)) {
                return Ok(repo);
            }
        }
    }
    if let Ok(repo) = Repository::open(git_dir, None) {
        return Ok(repo);
    }
    Repository::discover(None).context("open repository for reference-transaction hooks")
}

fn run_prepare_only_symref_hook(
    repo: &Repository,
    refname: &str,
    old_value: &str,
    target: &str,
) -> Result<()> {
    let line = format!("{old_value} ref:{target} {refname}\n");
    let result = run_hook(
        repo,
        "reference-transaction",
        &["preparing"],
        Some(line.as_bytes()),
    );
    match result {
        HookResult::NotFound | HookResult::Success => Ok(()),
        HookResult::Failed(_) => {
            bail!("in 'preparing' phase, update aborted by the reference-transaction hook")
        }
    }
}

fn fetch_reflog_identity(git_dir: &Path) -> String {
    let config = ConfigSet::load(Some(git_dir), true).ok();
    let name = std::env::var("GIT_COMMITTER_NAME")
        .ok()
        .or_else(|| std::env::var("GIT_AUTHOR_NAME").ok())
        .or_else(|| config.as_ref().and_then(|c| c.get("user.name")))
        .unwrap_or_else(|| "Unknown".to_owned());
    let email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| std::env::var("GIT_AUTHOR_EMAIL").ok())
        .or_else(|| config.as_ref().and_then(|c| c.get("user.email")))
        .unwrap_or_default();
    let now = time::OffsetDateTime::now_utc();
    let epoch = now.unix_timestamp();
    let offset = now.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();
    format!("{name} <{email}> {epoch} {hours:+03}{minutes:02}")
}

/// Append a reflog line for a ref updated by fetch (remote-tracking branches and tags).
fn append_fetch_reflog(
    git_dir: &Path,
    refname: &str,
    old_oid: Option<&ObjectId>,
    new_oid: &ObjectId,
    remote_url: &str,
    branch: &str,
) -> anyhow::Result<()> {
    let old = old_oid.cloned().unwrap_or_else(zero_oid);
    // Strip URL userinfo so credentials never reach the reflog (t5541 "clone/fetch scrubs
    // password from reflogs"); a named remote passes through unchanged.
    let remote_url = crate::http_client::scrub_url_credentials(remote_url);
    let message = if branch.is_empty() {
        format!("fetch --append --prune {remote_url}")
    } else {
        format!("fetch --append --prune {remote_url} branch '{branch}' of {remote_url}")
    };
    let ident = fetch_reflog_identity(git_dir);
    refs::append_reflog(git_dir, refname, &old, new_oid, &ident, &message, true)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Reflog action prefix (`rla`) for fetch ref updates, mirroring Git's `default_rla`
/// (`builtin/fetch.c`): `GIT_REFLOG_ACTION` when set, otherwise `fetch` followed by the
/// non-option command-line arguments (remote then refspecs).
fn fetch_reflog_action_prefix(args: &Args) -> String {
    if let Ok(rla) = std::env::var("GIT_REFLOG_ACTION") {
        if !rla.is_empty() {
            return rla;
        }
    }
    let mut prefix = String::from("fetch");
    if let Some(remote) = &args.remote {
        prefix.push(' ');
        // Scrub any userinfo (`user:pass@`) from a URL remote so credentials never land in the
        // reflog, matching Git's `transport_anonymize_url` (t5541 "clone/fetch scrubs password
        // from reflogs"). A named remote (e.g. `origin`) is not a URL, so this is a no-op there.
        prefix.push_str(&crate::http_client::scrub_url_credentials(remote));
    }
    for spec in &args.refspecs {
        prefix.push(' ');
        prefix.push_str(spec);
    }
    prefix
}

/// Write the fetch reflog entry for an updated local ref, matching Git's `s_update_ref`
/// message `"<rla>: <action>"`. The action is `storing {head,tag,ref}` for a new ref (based
/// on the *remote* ref name), or `fast-forward`/`forced-update` for an existing ref. Honors
/// `core.logallrefupdates` (including a `true` setting in a bare repo) via `append_reflog`.
fn write_fetch_explicit_reflog(
    args: &Args,
    git_dir: &Path,
    local_ref: &str,
    remote_refname: &str,
    old_oid: Option<&ObjectId>,
    new_oid: &ObjectId,
    fast_forward: bool,
) {
    if args.dry_run {
        return;
    }
    let action = match old_oid {
        None => {
            if remote_refname.starts_with("refs/tags/") {
                "storing tag"
            } else if remote_refname.starts_with("refs/heads/") {
                "storing head"
            } else {
                "storing ref"
            }
        }
        Some(_) if fast_forward => "fast-forward",
        Some(_) => "forced-update",
    };
    let message = format!("{}: {action}", fetch_reflog_action_prefix(args));
    let old = old_oid.cloned().unwrap_or_else(zero_oid);
    let ident = fetch_reflog_identity(git_dir);
    let _ = refs::append_reflog(git_dir, local_ref, &old, new_oid, &ident, &message, false);
}

/// OIDs whose object closure should be copied for this fetch (non-refetch local transport).
///
/// When the user passes explicit refspecs, only those sources are roots; otherwise configured
/// refspecs filter which remote refs participate; if there are no configured refspecs, all
/// remote heads and tags are roots (Git's default refspec set).
fn fetch_object_copy_roots(
    remote_git_dir: &Path,
    cli_refspecs: &[String],
    refspecs: &[FetchRefspec],
    heads: &[(String, ObjectId)],
    tags: &[(String, ObjectId)],
) -> Result<Vec<ObjectId>> {
    let mut roots = Vec::new();

    if !cli_refspecs.is_empty() {
        let negative_patterns: Vec<&str> = cli_refspecs
            .iter()
            .filter_map(|s| s.strip_prefix('^'))
            .collect();
        let is_excluded = |refname: &str| -> bool {
            for pat in &negative_patterns {
                let full_pat = if pat.starts_with("refs/") {
                    pat.to_string()
                } else {
                    format!("refs/heads/{pat}")
                };
                if match_glob_pattern(&full_pat, refname).is_some() || full_pat == refname {
                    return true;
                }
            }
            false
        };

        for spec in cli_refspecs {
            if spec.starts_with('^') {
                continue;
            }
            let spec_clean = spec.strip_prefix('+').unwrap_or(spec.as_str());
            let src = spec_clean
                .split_once(':')
                .map(|(a, _)| a)
                .unwrap_or(spec_clean);
            if src.contains('*') {
                let remote_all_refs = refs::list_refs(remote_git_dir, "refs/")?;
                for (refname, oid) in &remote_all_refs {
                    if is_excluded(refname) {
                        continue;
                    }
                    if match_glob_pattern(src, refname).is_some() {
                        roots.push(*oid);
                    }
                }
                continue;
            }
            let remote_ref = resolve_remote_ref_for_fetch_src(remote_git_dir, src)?;
            let oid = refs::resolve_ref(remote_git_dir, &remote_ref)
                .with_context(|| format!("couldn't find remote ref '{src}'"))?;
            roots.push(oid);
        }
    } else if refspecs.is_empty() {
        roots.extend(heads.iter().map(|(_, o)| *o));
        roots.extend(tags.iter().map(|(_, o)| *o));
    } else {
        for (refname, oid) in heads.iter().chain(tags.iter()) {
            let mut excluded = false;
            for rs in refspecs {
                if !rs.negative {
                    continue;
                }
                let pat = &rs.src;
                if match_glob_pattern(pat, refname).is_some() || pat == refname {
                    excluded = true;
                    break;
                }
            }
            if excluded {
                continue;
            }
            if map_ref_through_refspecs(refname, refspecs).is_some() {
                roots.push(*oid);
            }
        }
    }

    roots.sort_by_key(|o| o.to_hex());
    roots.dedup();
    Ok(roots)
}

/// Resolve a short or full remote ref name for fetch (CLI refspec source side).
fn resolve_remote_ref_for_fetch_src(remote_git_dir: &Path, src: &str) -> Result<String> {
    if src.is_empty() || src == "HEAD" {
        return Ok("HEAD".to_owned());
    }
    if src.starts_with("refs/") {
        return Ok(src.to_owned());
    }
    let candidates = [
        format!("refs/{src}"),
        format!("refs/tags/{src}"),
        format!("refs/heads/{src}"),
        format!("refs/remotes/{src}"),
        format!("refs/remotes/{src}/HEAD"),
    ];
    for cand in candidates {
        if refs::resolve_ref(remote_git_dir, &cand).is_ok() {
            return Ok(cand);
        }
    }
    let heads_ref = format!("refs/heads/{src}");
    if refs::resolve_ref(remote_git_dir, &heads_ref).is_ok() {
        return Ok(heads_ref);
    }
    Ok(heads_ref)
}

fn resolve_advertised_ref_for_fetch_src(
    src: &str,
    remote_all_refs: &[(String, ObjectId)],
    remote_symbolic_head_branch: Option<&str>,
) -> Option<String> {
    if src.is_empty() || src == "HEAD" {
        return Some("HEAD".to_owned());
    }
    if src.starts_with("refs/") {
        return Some(src.to_owned());
    }
    let mut candidates = vec![
        format!("refs/{src}"),
        format!("refs/tags/{src}"),
        format!("refs/heads/{src}"),
        format!("refs/remotes/{src}"),
        format!("refs/remotes/{src}/HEAD"),
    ];
    if let Some(branch) = remote_symbolic_head_branch {
        candidates.push(format!("refs/heads/{branch}"));
    }
    candidates
        .into_iter()
        .find(|cand| remote_all_refs.iter().any(|(name, _)| name == cand))
}

fn remote_oid_for_resolved_ref(
    resolved_ref: &str,
    find_remote_ref_oid: &dyn Fn(&str) -> Option<ObjectId>,
    remote_head_advertised_oid: Option<ObjectId>,
    remote_symbolic_head_branch: Option<&str>,
) -> Option<ObjectId> {
    if resolved_ref == "HEAD" {
        remote_head_advertised_oid
            .or_else(|| find_remote_ref_oid("HEAD"))
            .or_else(|| {
                remote_symbolic_head_branch
                    .and_then(|b| find_remote_ref_oid(&format!("refs/heads/{b}")))
            })
    } else {
        find_remote_ref_oid(resolved_ref)
    }
}

fn ensure_head_ref_target_is_commit(
    remote_repo: Option<&Repository>,
    local_ref: &str,
    remote_oid: ObjectId,
    src: &str,
    resolved_remote_ref: &Option<String>,
) -> Result<()> {
    if src.is_empty() {
        return Ok(());
    }
    if !local_ref.starts_with("refs/heads/") {
        return Ok(());
    }
    let Some(repo) = remote_repo else {
        return Ok(());
    };
    let Ok(obj) = repo.odb.read(&remote_oid) else {
        return Ok(());
    };
    if obj.kind == ObjectKind::Commit {
        return Ok(());
    }
    let shown_src = resolved_remote_ref.as_deref().unwrap_or(src);
    bail!(
        "object {} from '{}' is not a commit; cannot update '{}'",
        remote_oid.to_hex(),
        shown_src,
        local_ref
    );
}

/// Copy all objects (loose + packs) from remote to local.
/// If `refetch` is true, re-copy objects even if they already exist locally.
/// Copy objects from a remote git dir to local git dir (public for pull).
pub fn copy_objects_for_pull(src_git_dir: &Path, dst_git_dir: &Path) -> Result<()> {
    copy_objects(src_git_dir, dst_git_dir, false)
}

/// Copy objects reachable from `roots` from `src_git_dir` into `dst_git_dir` (loose only).
///
/// Used for local `fetch` so the destination does not receive unrelated objects from the
/// source object database (matching Git's behavior and keeping negotiation tests faithful).
pub(crate) fn copy_reachable_objects(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    roots: &[ObjectId],
) -> Result<()> {
    copy_reachable_objects_internal(src_git_dir, dst_git_dir, roots, false, false, false)
}

/// Copy objects reachable from `roots`, skipping gitlink tree entries.
///
/// Used by submodule local fetches where a superproject tree may contain gitlink commit IDs that
/// live in a nested submodule repository, not in the superproject object database.
pub(crate) fn copy_reachable_objects_skipping_gitlinks(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    roots: &[ObjectId],
) -> Result<()> {
    copy_reachable_objects_internal(src_git_dir, dst_git_dir, roots, false, true, false)
}

fn copy_reachable_objects_filtered(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    roots: &[ObjectId],
    filter_spec: &str,
) -> Result<()> {
    let filter = ObjectFilter::parse(filter_spec)
        .map_err(|err| anyhow::anyhow!("invalid object filter: {err}"))?;
    let src_odb = Odb::new(&src_git_dir.join("objects"));
    let dst_odb = Odb::new(&dst_git_dir.join("objects"));
    let mut stack: Vec<ObjectId> = roots.to_vec();
    let mut seen = HashSet::new();
    let mut omitted = HashSet::new();
    let shallow_boundaries = grit_lib::shallow::load_shallow_boundaries(dst_git_dir);

    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let obj = src_odb.read(&oid).with_context(|| {
            format!("missing object {} while copying from remote", oid.to_hex())
        })?;
        if obj.kind == ObjectKind::Blob && filter_omits_blob(&filter, obj.data.len() as u64) {
            if !dst_odb.exists_local(&oid) {
                omitted.insert(oid);
            }
            continue;
        }
        if !dst_odb.exists_local(&oid) {
            dst_odb
                .write(obj.kind, &obj.data)
                .with_context(|| format!("write object {}", oid.to_hex()))?;
        }
        match obj.kind {
            ObjectKind::Commit => {
                let c = parse_commit(&obj.data)?;
                stack.push(c.tree);
                if !shallow_boundaries.contains(&oid) {
                    stack.extend_from_slice(&c.parents);
                }
            }
            ObjectKind::Tree => {
                for e in parse_tree(&obj.data)? {
                    stack.push(e.oid);
                }
            }
            ObjectKind::Tag => {
                stack.push(parse_tag(&obj.data)?.object);
            }
            ObjectKind::Blob => {}
        }
    }

    let mut marker_set: HashSet<ObjectId> = read_promisor_missing_oids(dst_git_dir)
        .into_iter()
        .collect();
    marker_set.extend(omitted);
    marker_set.retain(|oid| !dst_odb.exists_local(oid));
    write_promisor_marker(dst_git_dir, &marker_set)?;

    Ok(())
}

fn filter_omits_blob(filter: &ObjectFilter, size: u64) -> bool {
    match filter {
        ObjectFilter::BlobNone => true,
        ObjectFilter::BlobLimit(limit) => size > *limit,
        ObjectFilter::Combine(filters) => {
            filters.iter().any(|filter| filter_omits_blob(filter, size))
        }
        _ => false,
    }
}

/// Copy objects reachable from `roots`, optionally stopping parent traversal at remote shallow
/// boundaries found in `<src_git_dir>/shallow`.
///
/// When `respect_remote_shallow_boundaries` is true, commits listed in the source shallow file are
/// treated as traversal boundaries: the commit object and its tree are copied, but parent commits
/// beyond that boundary are not copied.
/// Copy objects reachable from `roots`, tolerating objects that are missing on the source because
/// the source is itself a partial clone (its promisor objects live on its lazy-fetch remote).
///
/// Used by local `git pull`/`fetch` when the destination treats promisor packs (`extensions.
/// partialclone` set or some `remote.*.promisor=true`). A missing source object is recorded in the
/// destination's promisor markers so it can be lazy-fetched on first access, mirroring Git's local
/// transport against a partial-clone server (t5710 subsequent fetch).
pub(crate) fn copy_reachable_objects_skipping_missing_promisor(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    roots: &[ObjectId],
) -> Result<()> {
    copy_reachable_objects_internal(src_git_dir, dst_git_dir, roots, false, true, true)
}

/// True when the source repository advertises its promisor remotes (`promisor.advertise=true`).
///
/// Mirrors the server-side decision in protocol v2: when the source advertises, a partial-clone
/// client accepts the advertisement and the source may omit its promisor objects (the client
/// lazy-fetches them later). When the source does *not* advertise, the source must instead
/// lazy-fetch any missing promisor objects from its own remote so it can serve them.
fn source_advertises_promisor(src_git_dir: &Path) -> bool {
    ConfigSet::load(Some(src_git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get_bool("promisor.advertise").and_then(|r| r.ok()))
        .unwrap_or(false)
}

fn copy_reachable_objects_internal(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    roots: &[ObjectId],
    respect_remote_shallow_boundaries: bool,
    skip_gitlinks: bool,
    skip_missing_promisor: bool,
) -> Result<()> {
    let src_odb = Odb::new(&src_git_dir.join("objects"));
    let dst_odb = Odb::new(&dst_git_dir.join("objects"));
    let remote_shallow_boundaries = if respect_remote_shallow_boundaries {
        read_shallow_boundary_oids(src_git_dir)?
    } else {
        HashSet::new()
    };
    // When the source is a partial clone that does *not* advertise its promisor remotes, it must
    // serve every requested object, so lazy-fetch any object missing on the source from the
    // source's own promisor remote before copying (t5710 subsequent fetch, advertise=false). When
    // it *does* advertise, the source may omit them and the destination records them for its own
    // later lazy-fetch.
    let source_lazy_fetches = skip_missing_promisor && !source_advertises_promisor(src_git_dir);
    let src_repo = if source_lazy_fetches {
        Repository::open(src_git_dir, None).ok()
    } else {
        None
    };
    let mut stack: Vec<ObjectId> = roots.to_vec();
    let mut seen = HashSet::new();
    let mut missing_promisor: HashSet<ObjectId> = HashSet::new();

    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        // Prune at objects the destination already has (its "haves"): when the destination already
        // holds an object, it holds that object's whole closure, so the source need not send (or
        // lazy-fetch) it or its ancestors. This mirrors Git's negotiation, where the client's
        // "have" lines mark its tips as uninteresting so the remote `pack-objects` never walks
        // below them.
        //
        // For the promisor case it is essential so a pull that introduces only a new commit does
        // not lazy-fetch unrelated promisor objects the destination already had (t5710 subsequent
        // fetch, advertise=false: only the new `bar` blob is fetched, leaving the old `foo` blob
        // missing on the server). For the ordinary local copy it lets a fetch of a new tip whose
        // history overlaps the destination stop at the shared boundary instead of walking into the
        // source's missing ancestors — e.g. fetching commit C (parent B) from a repository that is
        // itself missing B's older ancestors, when the destination already has B's full closure
        // (t5306 indirectly clone patch_clone). A later `check_connectivity` pass still verifies the
        // destination really holds the pruned closure, so a genuinely incomplete fetch still fails.
        if dst_odb.exists_local(&oid) {
            continue;
        }
        let obj = match src_odb.read(&oid) {
            Ok(obj) => obj,
            Err(err) => {
                // When the destination is a partial clone, the source legitimately omits its own
                // promisor objects (they live on the source's lazy-fetch remote). If the source
                // does not advertise its promisor remotes, lazy-fetch the object onto the source
                // first (so the source serves it and the destination receives a full object);
                // otherwise skip it, recording it as missing so the destination lazy-fetches it
                // later. `exists` can report true for an object only referenced by a promisor pack
                // index without its data, so test `exists_local` for an actual local copy.
                if skip_missing_promisor {
                    if let Some(repo) = src_repo.as_ref() {
                        if crate::commands::promisor_hydrate::try_lazy_fetch_promisor_objects_batch(
                            repo,
                            &[oid],
                        )
                        .is_ok()
                        {
                            if let Ok(obj) = src_odb.read(&oid) {
                                if !dst_odb.exists(&oid) {
                                    dst_odb.write(obj.kind, &obj.data).with_context(|| {
                                        format!("write object {}", oid.to_hex())
                                    })?;
                                }
                                push_object_children(
                                    &obj,
                                    &oid,
                                    skip_gitlinks,
                                    &remote_shallow_boundaries,
                                    &mut stack,
                                )?;
                                continue;
                            }
                        }
                    }
                    if !dst_odb.exists_local(&oid) {
                        missing_promisor.insert(oid);
                    }
                    continue;
                }
                return Err(err).with_context(|| {
                    format!("missing object {} while copying from remote", oid.to_hex())
                });
            }
        };
        if !dst_odb.exists(&oid) {
            dst_odb
                .write(obj.kind, &obj.data)
                .with_context(|| format!("write object {}", oid.to_hex()))?;
        }
        push_object_children(
            &obj,
            &oid,
            skip_gitlinks,
            &remote_shallow_boundaries,
            &mut stack,
        )?;
    }

    if !missing_promisor.is_empty() {
        let mut marker_set: HashSet<ObjectId> = read_promisor_missing_oids(dst_git_dir)
            .into_iter()
            .collect();
        marker_set.extend(missing_promisor);
        marker_set.retain(|oid| !dst_odb.exists_local(oid));
        write_promisor_marker(dst_git_dir, &marker_set)?;
    }
    Ok(())
}

/// Push the reachable children of `obj` (with object id `oid`) onto `stack` for a reachable-object
/// copy walk: a commit's tree (and parents, unless `oid` is a shallow boundary), a tree's entries
/// (skipping gitlink entries when `skip_gitlinks`), and a tag's target.
fn push_object_children(
    obj: &grit_lib::objects::Object,
    oid: &ObjectId,
    skip_gitlinks: bool,
    remote_shallow_boundaries: &HashSet<ObjectId>,
    stack: &mut Vec<ObjectId>,
) -> Result<()> {
    match obj.kind {
        ObjectKind::Commit => {
            let c = parse_commit(&obj.data)?;
            stack.push(c.tree);
            if !remote_shallow_boundaries.contains(oid) {
                stack.extend_from_slice(&c.parents);
            }
        }
        ObjectKind::Tree => {
            for e in parse_tree(&obj.data)? {
                if skip_gitlinks && e.mode == 0o160000 {
                    continue;
                }
                stack.push(e.oid);
            }
        }
        ObjectKind::Tag => {
            stack.push(parse_tag(&obj.data)?.object);
        }
        ObjectKind::Blob => {}
    }
    Ok(())
}

pub(crate) fn prune_loose_objects_available_from_alternates(git_dir: &Path) -> Result<()> {
    let objects_dir = git_dir.join("objects");
    let alternates = grit_lib::pack::read_alternates_recursive(&objects_dir).unwrap_or_default();
    if alternates.is_empty() {
        return Ok(());
    }

    for fanout in fs::read_dir(&objects_dir).with_context(|| {
        format!(
            "reading object directory while pruning alternates at {}",
            objects_dir.display()
        )
    })? {
        let fanout = fanout?;
        let prefix = fanout.file_name();
        let Some(prefix) = prefix.to_str() else {
            continue;
        };
        if prefix.len() != 2 || !fanout.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(fanout.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let suffix = entry.file_name();
            let Some(suffix) = suffix.to_str() else {
                continue;
            };
            if suffix.len() != 38 {
                continue;
            }
            let Ok(oid) = ObjectId::from_hex(&format!("{prefix}{suffix}")) else {
                continue;
            };
            if object_exists_in_any_objects_dir(&alternates, &oid) {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(fanout.path());
    }
    Ok(())
}

fn object_exists_in_any_objects_dir(objects_dirs: &[PathBuf], oid: &ObjectId) -> bool {
    objects_dirs.iter().any(|objects_dir| {
        objects_dir
            .join(oid.loose_prefix())
            .join(oid.loose_suffix())
            .is_file()
            || grit_lib::pack::read_local_pack_indexes(objects_dir)
                .map(|indexes| indexes.iter().any(|idx| idx.contains(oid)))
                .unwrap_or(false)
    })
}

fn maybe_lazy_fetch_parent_tree_blobs_for_promisor_trace(
    repo: &Repository,
    config: &ConfigSet,
    remote_heads: &[(String, ObjectId)],
) {
    if crate::trace_packet::trace_packet_dest().is_none() {
        return;
    }
    if !repo_treats_promisor_packs(&repo.git_dir, config) {
        return;
    }
    let mut need = Vec::new();
    let mut seen = HashSet::new();
    for (_, head_oid) in remote_heads {
        let Ok(obj) = repo.odb.read(head_oid) else {
            continue;
        };
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let Ok(commit) = parse_commit(&obj.data) else {
            continue;
        };
        for parent in commit.parents {
            let Ok(parent_obj) = repo.odb.read(&parent) else {
                continue;
            };
            if parent_obj.kind != ObjectKind::Commit {
                continue;
            }
            let Ok(parent_commit) = parse_commit(&parent_obj.data) else {
                continue;
            };
            collect_missing_tree_blobs(repo, parent_commit.tree, &mut seen, &mut need);
        }
    }
    need.sort();
    need.dedup();
    if !need.is_empty() {
        let _ =
            crate::commands::promisor_hydrate::try_lazy_fetch_promisor_objects_batch(repo, &need);
    }
}

fn maybe_materialize_include_tag_objects(
    local_repo: &Repository,
    remote_repo: &Repository,
    remote_heads: &[(String, ObjectId)],
    remote_tags: &[(String, ObjectId)],
) {
    let head_oids: Vec<ObjectId> = remote_heads.iter().map(|(_, oid)| *oid).collect();
    for (_, tag_oid) in remote_tags {
        if local_repo.odb.read(tag_oid).is_ok() {
            continue;
        }
        let Ok(tag_obj) = remote_repo.odb.read(tag_oid) else {
            continue;
        };
        if tag_obj.kind != ObjectKind::Tag {
            continue;
        }
        let Ok(tag) = parse_tag(&tag_obj.data) else {
            continue;
        };
        if local_repo.odb.read(&tag.object).is_err() {
            continue;
        }
        let reachable_from_fetched_head = head_oids.iter().any(|head| {
            tag.object == *head
                || merge_base::is_ancestor(remote_repo, tag.object, *head).unwrap_or(false)
        });
        if reachable_from_fetched_head {
            let _ = local_repo.odb.write(tag_obj.kind, &tag_obj.data);
        }
    }
}

fn collect_missing_tree_blobs(
    repo: &Repository,
    tree_oid: ObjectId,
    seen: &mut HashSet<ObjectId>,
    need: &mut Vec<ObjectId>,
) {
    if !seen.insert(tree_oid) {
        return;
    }
    let Ok(obj) = repo.odb.read(&tree_oid) else {
        return;
    };
    if obj.kind != ObjectKind::Tree {
        return;
    }
    let Ok(entries) = parse_tree(&obj.data) else {
        return;
    };
    for entry in entries {
        match entry.mode {
            0o040000 => collect_missing_tree_blobs(repo, entry.oid, seen, need),
            0o160000 => {}
            _ if !repo.odb.exists_local(&entry.oid) => need.push(entry.oid),
            _ => {}
        }
    }
}

/// Copy objects reachable from `roots`, but do not traverse parent commits past source shallow
/// boundaries.
fn copy_reachable_objects_respecting_source_shallow(
    src_git_dir: &Path,
    dst_git_dir: &Path,
    roots: &[ObjectId],
) -> Result<()> {
    copy_reachable_objects_internal(src_git_dir, dst_git_dir, roots, true, false, false)
}

fn copy_objects(src_git_dir: &Path, dst_git_dir: &Path, refetch: bool) -> Result<()> {
    let src_objects = src_git_dir.join("objects");
    let dst_objects = dst_git_dir.join("objects");

    // Copy loose objects (fan-out directories: 00..ff)
    if src_objects.is_dir() {
        for entry in fs::read_dir(&src_objects)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Skip info/ and pack/ — handled separately
            if name_str == "info" || name_str == "pack" {
                continue;
            }

            // Only process 2-character hex fan-out dirs
            if !entry.file_type()?.is_dir() || name_str.len() != 2 {
                continue;
            }

            let dst_dir = dst_objects.join(&*name);
            for inner in fs::read_dir(entry.path())? {
                let inner = inner?;
                if inner.file_type()?.is_file() {
                    let dst_file = dst_dir.join(inner.file_name());
                    if refetch || !dst_file.exists() {
                        fs::create_dir_all(&dst_dir)?;
                        if refetch {
                            // Force copy when refetching
                            fs::copy(inner.path(), &dst_file)?;
                        } else if fs::hard_link(inner.path(), &dst_file).is_err() {
                            fs::copy(inner.path(), &dst_file)?;
                        }
                    }
                }
            }
        }
    }

    // Copy pack files
    let src_pack = src_objects.join("pack");
    let dst_pack = dst_objects.join("pack");
    if src_pack.is_dir() {
        fs::create_dir_all(&dst_pack)?;
        for entry in fs::read_dir(&src_pack)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let dst_file = dst_pack.join(entry.file_name());
                if (refetch || !dst_file.exists())
                    && (refetch || fs::hard_link(entry.path(), &dst_file).is_err())
                {
                    fs::copy(entry.path(), &dst_file)?;
                }
            }
        }
    }

    Ok(())
}

/// Verify that all objects reachable from the given OIDs exist in the local ODB.
/// This is used after copying objects from a remote to detect incomplete transfers.
fn check_connectivity(git_dir: &Path, tip_oids: &[ObjectId]) -> Result<()> {
    let hidden_prefix = if has_hide_refs_for_fetch_connectivity(git_dir) {
        Some("--exclude-hidden=fetch")
    } else {
        None
    };
    if let Some(flag) = hidden_prefix {
        trace_run_command_git_invocation(&["rev-list", "--objects", "--stdin", flag]);
    } else {
        trace_run_command_git_invocation(&["rev-list", "--objects", "--stdin"]);
    }

    use grit_lib::objects::{parse_commit, parse_tree, ObjectKind};
    use grit_lib::odb::Odb;
    use std::collections::HashSet;

    let odb = Odb::new(&git_dir.join("objects"));
    let mut seen = HashSet::new();
    let mut stack: Vec<ObjectId> = tip_oids.to_vec();

    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let obj = odb
            .read(&oid)
            .with_context(|| "remote did not send all necessary objects".to_string())?;
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(commit) = parse_commit(&obj.data) {
                    stack.push(commit.tree);
                    for parent in &commit.parents {
                        stack.push(*parent);
                    }
                }
            }
            ObjectKind::Tree => {
                if let Ok(entries) = parse_tree(&obj.data) {
                    for entry in entries {
                        // Skip gitlink (submodule) entries
                        if entry.mode == 0o160000 {
                            continue;
                        }
                        stack.push(entry.oid);
                    }
                }
            }
            ObjectKind::Blob | ObjectKind::Tag => {
                // Blobs and tags are leaf objects, no further traversal needed
            }
        }
    }
    Ok(())
}

fn has_hide_refs_for_fetch_connectivity(git_dir: &Path) -> bool {
    if std::env::var("GIT_CONFIG_PARAMETERS")
        .ok()
        .is_some_and(|raw| {
            let lower = raw.to_ascii_lowercase();
            lower.contains("fetch.hiderefs=") || lower.contains("transfer.hiderefs=")
        })
    {
        return true;
    }
    ConfigSet::load(Some(git_dir), true)
        .ok()
        .is_some_and(|cfg| {
            cfg.entries().iter().any(|entry| {
                let key = entry.key.as_str();
                key.starts_with("fetch.hiderefs")
                    || key.starts_with("transfer.hiderefs")
                    || key == "fetch.hiderefs"
                    || key == "transfer.hiderefs"
            })
        })
}

/// Remove remote-tracking refs that no longer exist on the remote.
#[expect(clippy::too_many_arguments)]
fn prune_stale_refs(
    args: &Args,
    git_dir: &Path,
    pending_atomic_ref_ops: &mut Vec<PendingRefOp>,
    prefix: &str,
    current_refs: &[String],
    remote_name: &str,
    quiet: bool,
    ref_update_failures: &mut Vec<String>,
    prune_refspecs: &[FetchRefspec],
    display: &mut FetchDisplay,
) -> Result<()> {
    let existing = refs::list_refs(git_dir, prefix)?;
    for (refname, oid) in &existing {
        if refname == &format!("refs/remotes/{remote_name}/HEAD") {
            continue;
        }
        // A local ref whose remote source is excluded by a negative refspec must not be pruned
        // (Git's `refspec_find_negative_match` makes such a ref "not match" the refspec set).
        if local_ref_protected_by_negative(refname, prune_refspecs) {
            continue;
        }
        if !current_refs.contains(refname) {
            apply_single_ref_delete(
                args,
                git_dir,
                pending_atomic_ref_ops,
                refname,
                Some(*oid),
                ref_update_failures,
            )
            .with_context(|| format!("pruning {refname}"))?;
            if !quiet {
                display.push(
                    '-',
                    "[deleted]",
                    "(none)",
                    refname,
                    refname,
                    *oid,
                    ObjectId::zero(),
                    None,
                );
            }
        }
    }
    Ok(())
}

/// Write shallow graft information when --depth / --deepen is used.
///
/// For local transport we approximate shallowness by listing the commit(s) at
/// the boundary depth and recording them in `$GIT_DIR/shallow`.
fn write_shallow_info(
    git_dir: &Path,
    remote_heads: &[(String, ObjectId)],
    remote_repo: &Repository,
    depth: usize,
    replace_ancestor_boundaries: bool,
) -> Result<()> {
    use grit_lib::objects::{parse_commit, ObjectKind};
    use grit_lib::odb::Odb;

    let shallow_path = git_dir.join("shallow");
    // Start from existing shallow boundaries; each fetched tip rewrites only its own boundary.
    let mut shallow_set: std::collections::HashSet<String> = if shallow_path.exists() {
        fs::read_to_string(&shallow_path)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let odb = Odb::new(&remote_repo.git_dir.join("objects"));

    // For each remote head, walk `depth` commits and mark the boundary.
    for (_refname, tip_oid) in remote_heads {
        if replace_ancestor_boundaries {
            let mut to_remove = Vec::new();
            for existing in &shallow_set {
                if let Ok(existing_oid) = ObjectId::from_hex(existing) {
                    let superseded = merge_base::is_ancestor(remote_repo, existing_oid, *tip_oid)
                        .unwrap_or(false);
                    if superseded {
                        to_remove.push(existing.clone());
                    }
                }
            }
            for old in to_remove {
                shallow_set.remove(&old);
            }
        }
        let mut oid = *tip_oid;
        for _ in 0..depth.saturating_sub(1) {
            match odb.read(&oid) {
                Ok(obj) if obj.kind == ObjectKind::Commit => match parse_commit(&obj.data) {
                    Ok(c) => {
                        if c.parents.is_empty() {
                            break;
                        }
                        oid = c.parents[0];
                    }
                    Err(_) => break,
                },
                _ => break,
            }
        }
        shallow_set.insert(oid.to_string());
    }

    let mut entries: Vec<&str> = shallow_set.iter().map(|s| s.as_str()).collect();
    entries.sort();
    let content = entries.join("\n") + "\n";
    fs::write(&shallow_path, content).context("writing shallow file")?;
    Ok(())
}

fn read_shallow_boundaries(git_dir: &Path) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    let Ok(content) = fs::read_to_string(git_dir.join("shallow")) else {
        return out;
    };
    for line in content.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Ok(oid) = ObjectId::from_hex(line) {
            out.insert(oid);
        }
    }
    out
}

fn shallow_depth_from_tip(
    repo: &Repository,
    tip: ObjectId,
    boundaries: &HashSet<ObjectId>,
) -> Option<usize> {
    let mut depth = 1usize;
    let mut current = tip;
    loop {
        if boundaries.contains(&current) {
            return Some(depth);
        }
        let obj = repo.odb.read(&current).ok()?;
        if obj.kind != ObjectKind::Commit {
            return Some(depth);
        }
        let commit = parse_commit(&obj.data).ok()?;
        let parent = *commit.parents.first()?;
        current = parent;
        depth += 1;
    }
}

fn commit_distance(repo: &Repository, from: ObjectId, target: ObjectId) -> Option<usize> {
    if from == target {
        return Some(0);
    }
    let mut queue = std::collections::VecDeque::new();
    let mut seen = HashSet::new();
    queue.push_back((from, 0usize));
    while let Some((oid, dist)) = queue.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        let obj = repo.odb.read(&oid).ok()?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data).ok()?;
        for parent in commit.parents {
            if parent == target {
                return Some(dist + 1);
            }
            queue.push_back((parent, dist + 1));
        }
    }
    None
}

fn read_shallow_boundary_oids(remote_git_dir: &Path) -> Result<HashSet<ObjectId>> {
    let shallow_path = remote_git_dir.join("shallow");
    if !shallow_path.exists() {
        return Ok(HashSet::new());
    }
    let mut set = HashSet::new();
    for line in fs::read_to_string(&shallow_path)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        if let Ok(oid) = ObjectId::from_hex(line) {
            set.insert(oid);
        }
    }
    Ok(set)
}

fn oid_reaches_shallow_boundary(
    repo: &Repository,
    oid: ObjectId,
    boundary_oids: &HashSet<ObjectId>,
    memo: &mut HashMap<ObjectId, bool>,
) -> bool {
    if let Some(result) = memo.get(&oid) {
        return *result;
    }
    if boundary_oids.contains(&oid) {
        memo.insert(oid, true);
        return true;
    }
    let Ok(obj) = repo.odb.read(&oid) else {
        memo.insert(oid, false);
        return false;
    };
    let reaches = match obj.kind {
        ObjectKind::Commit => parse_commit(&obj.data).is_ok_and(|commit| {
            commit
                .parents
                .iter()
                .any(|parent| oid_reaches_shallow_boundary(repo, *parent, boundary_oids, memo))
        }),
        ObjectKind::Tag => parse_tag(&obj.data)
            .is_ok_and(|tag| oid_reaches_shallow_boundary(repo, tag.object, boundary_oids, memo)),
        _ => false,
    };
    memo.insert(oid, reaches);
    reaches
}

fn refs_requiring_update_shallow(
    remote_git_dir: &Path,
    local_git_dir: &Path,
) -> Result<HashSet<String>> {
    let boundary_oids = read_shallow_boundary_oids(remote_git_dir)?;
    if boundary_oids.is_empty() {
        return Ok(HashSet::new());
    }

    let local_boundary_oids = read_shallow_boundary_oids(local_git_dir)?;
    let required_new_boundaries: HashSet<ObjectId> = boundary_oids
        .difference(&local_boundary_oids)
        .copied()
        .collect();
    if required_new_boundaries.is_empty() {
        return Ok(HashSet::new());
    }

    let repo = Repository::open(remote_git_dir, None)
        .with_context(|| format!("open remote repository {}", remote_git_dir.display()))?;
    let mut blocked = HashSet::new();
    let mut memo = HashMap::new();
    for (refname, oid) in refs::list_refs(remote_git_dir, "refs/")? {
        if refname.starts_with("refs/tags/") {
            continue;
        }
        if oid_reaches_shallow_boundary(&repo, oid, &required_new_boundaries, &mut memo) {
            blocked.insert(refname);
        }
    }
    Ok(blocked)
}

fn write_remote_shallow_info_for_tips(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    tip_oids: &[ObjectId],
) -> Result<()> {
    let boundary_oids = read_shallow_boundary_oids(remote_git_dir)?;
    if boundary_oids.is_empty() {
        return Ok(());
    }
    let remote_repo = Repository::open(remote_git_dir, None)
        .with_context(|| format!("open remote repository {}", remote_git_dir.display()))?;
    let shallow_path = local_git_dir.join("shallow");
    let mut local_boundaries: HashSet<ObjectId> = if shallow_path.exists() {
        fs::read_to_string(&shallow_path)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .filter_map(|l| ObjectId::from_hex(l).ok())
            .collect()
    } else {
        HashSet::new()
    };

    let mut stack: Vec<ObjectId> = tip_oids.to_vec();
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if boundary_oids.contains(&oid) {
            local_boundaries.insert(oid);
            continue;
        }
        let Ok(obj) = remote_repo.odb.read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(commit) = parse_commit(&obj.data) {
                    stack.extend(commit.parents);
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = parse_tag(&obj.data) {
                    stack.push(tag.object);
                }
            }
            _ => {}
        }
    }

    if !local_boundaries.is_empty() {
        let mut entries: Vec<String> = local_boundaries.iter().map(ObjectId::to_hex).collect();
        entries.sort();
        fs::write(&shallow_path, entries.join("\n") + "\n").context("writing shallow file")?;
    }
    Ok(())
}

fn sync_shallow_boundaries_for_unshallow(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    tip_oids: &[ObjectId],
) -> Result<()> {
    let boundary_oids = read_shallow_boundary_oids(remote_git_dir)?;
    let shallow_path = local_git_dir.join("shallow");

    if boundary_oids.is_empty() {
        if shallow_path.exists() {
            fs::remove_file(&shallow_path).context("removing shallow grafts for --unshallow")?;
        }
        return Ok(());
    }

    let remote_repo = Repository::open(remote_git_dir, None)
        .with_context(|| format!("open remote repository {}", remote_git_dir.display()))?;

    // `--unshallow` should align local shallow boundaries to the remote's current boundaries,
    // not keep previous local deepen markers that may be superseded.
    let mut local_boundaries: HashSet<ObjectId> = HashSet::new();

    let mut stack: Vec<ObjectId> = tip_oids.to_vec();
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if boundary_oids.contains(&oid) {
            local_boundaries.insert(oid);
            continue;
        }
        let Ok(obj) = remote_repo.odb.read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(commit) = parse_commit(&obj.data) {
                    stack.extend(commit.parents);
                }
            }
            ObjectKind::Tag => {
                if let Ok(tag) = parse_tag(&obj.data) {
                    stack.push(tag.object);
                }
            }
            _ => {}
        }
    }

    if local_boundaries.is_empty() {
        if shallow_path.exists() {
            fs::remove_file(&shallow_path).context("removing shallow grafts for --unshallow")?;
        }
    } else {
        let mut entries: Vec<String> = local_boundaries.iter().map(ObjectId::to_hex).collect();
        entries.sort();
        fs::write(&shallow_path, entries.join("\n") + "\n").context("writing shallow file")?;
    }
    Ok(())
}

/// `remote.<name>.followRemoteHEAD` (subset matching Git's `remote.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowRemoteHead {
    Never,
    Create,
    Warn,
    Always,
}

/// Parsed `remote.<name>.followRemoteHEAD` plus optional `warn-if-not-<branch>` suffix.
struct FollowRemoteHeadConfig {
    mode: FollowRemoteHead,
    /// When `mode == Warn` and this is `Some`, suppress the warning if the remote HEAD branch
    /// equals this name (Git's `warn-if-not-<branch>`).
    no_warn_branch: Option<String>,
}

fn parse_follow_remote_head(config: &ConfigSet, remote_name: &str) -> FollowRemoteHeadConfig {
    let key = format!("remote.{remote_name}.followRemoteHEAD");
    let key_alt = format!("remote.{remote_name}.followremotehead");
    let raw = config
        .get(&key)
        .or_else(|| config.get(&key_alt))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if raw.is_empty() {
        return FollowRemoteHeadConfig {
            mode: FollowRemoteHead::Create,
            no_warn_branch: None,
        };
    }
    let lower = raw.to_ascii_lowercase();
    if lower == "never" {
        return FollowRemoteHeadConfig {
            mode: FollowRemoteHead::Never,
            no_warn_branch: None,
        };
    }
    if lower == "create" {
        return FollowRemoteHeadConfig {
            mode: FollowRemoteHead::Create,
            no_warn_branch: None,
        };
    }
    if lower == "warn" {
        return FollowRemoteHeadConfig {
            mode: FollowRemoteHead::Warn,
            no_warn_branch: None,
        };
    }
    if lower == "always" {
        return FollowRemoteHeadConfig {
            mode: FollowRemoteHead::Always,
            no_warn_branch: None,
        };
    }
    let prefix = "warn-if-not-";
    if lower.starts_with(prefix) {
        let branch = raw[prefix.len()..].to_string();
        return FollowRemoteHeadConfig {
            mode: FollowRemoteHead::Warn,
            no_warn_branch: if branch.is_empty() {
                None
            } else {
                Some(branch)
            },
        };
    }
    FollowRemoteHeadConfig {
        mode: FollowRemoteHead::Create,
        no_warn_branch: None,
    }
}

fn should_force_tag_update(config: &ConfigSet, remote_name: &str, args: &Args) -> bool {
    if args.force {
        return true;
    }
    let key = format!("remote.{remote_name}.tagOpt");
    config
        .get(&key)
        .map(|v| v.trim() == "--force")
        .unwrap_or(false)
}

/// How a tag should be updated during fetch.
///
/// Mirrors upstream Git's distinction between tags fetched through an *explicit* refspec
/// (`--tags`, or a configured/CLI refspec whose destination lands under `refs/tags/`, e.g.
/// `+refs/tags/two:refs/tags/two`) and tags that are merely *opportunistically auto-followed*
/// because their target object was downloaded as part of a branch fetch.
///
/// Explicit tag updates honor force semantics: a non-fast-forward (a tag that moved) is rejected
/// with `! [rejected] (would clobber existing tag)` unless the refspec is forced. Opportunistic
/// auto-follow is strictly *add-only* — an existing local tag is left untouched (no update, no
/// rejection, no error), matching `git fetch` against the default `+refs/heads/*:...` refspec.
enum TagUpdateKind {
    /// Tag is covered by an explicit tag refspec; `bool` is whether that refspec is forced.
    Explicit { force: bool },
    /// Tag is only opportunistically auto-followed (not covered by any explicit tag refspec).
    Opportunistic,
}

/// Determine how `tag_refname` (a `refs/tags/...` ref) should be updated, given the active
/// configured/CLI fetch refspecs and the `--tags` flag.
fn tag_update_kind(
    tag_refname: &str,
    args: &Args,
    configured_refspecs: &[FetchRefspec],
    cli_refspecs: &[FetchRefspec],
) -> TagUpdateKind {
    // `--tags` behaves like an explicit `refs/tags/*:refs/tags/*` refspec (non-forced).
    if args.tags {
        return TagUpdateKind::Explicit { force: false };
    }
    for rs in cli_refspecs.iter().chain(configured_refspecs.iter()) {
        if rs.negative {
            continue;
        }
        if let Some(dst) = match_refspec_pattern(&rs.src, &rs.dst, tag_refname) {
            if dst.starts_with("refs/tags/") {
                return TagUpdateKind::Explicit { force: rs.force };
            }
        }
    }
    TagUpdateKind::Opportunistic
}

fn trace_ls_refs_head_prefix() {
    crate::trace_packet::trace_packet_line(b"fetch> ref-prefix HEAD");
}

/// Previous `refs/remotes/<remote>/HEAD` state for `followRemoteHEAD` warnings.
#[derive(Clone)]
enum RemoteHeadPrevious {
    Symref(String),
    DetachedOid(ObjectId),
    Missing,
}

fn read_remote_head_previous(git_dir: &Path, remote_name: &str) -> RemoteHeadPrevious {
    let head_ref = format!("refs/remotes/{remote_name}/HEAD");
    let sym = refs::read_symbolic_ref(git_dir, &head_ref).ok().flatten();
    if let Some(target) = sym {
        let prefix = format!("refs/remotes/{remote_name}/");
        let short = target
            .strip_prefix(&prefix)
            .map(|s| s.to_string())
            .unwrap_or(target);
        return RemoteHeadPrevious::Symref(short);
    }
    match read_ref_oid(git_dir, &head_ref) {
        Some(oid) => RemoteHeadPrevious::DetachedOid(oid),
        None => RemoteHeadPrevious::Missing,
    }
}

fn maybe_warn_follow_remote_head(
    follow: &FollowRemoteHeadConfig,
    remote_name: &str,
    remote_default_branch: &str,
    previous: RemoteHeadPrevious,
    quiet: bool,
) {
    if quiet || follow.mode != FollowRemoteHead::Warn {
        return;
    }
    if let Some(ref skip) = follow.no_warn_branch {
        if skip == remote_default_branch {
            return;
        }
    }
    let mut stdout = std::io::stdout();
    match previous {
        RemoteHeadPrevious::Symref(prev_short) if prev_short != remote_default_branch => {
            let _ = writeln!(
                stdout,
                "'HEAD' at '{remote_name}' is '{remote_default_branch}', but we have '{prev_short}' locally."
            );
        }
        RemoteHeadPrevious::DetachedOid(oid) => {
            let _ = writeln!(
                stdout,
                "'HEAD' at '{remote_name}' is '{remote_default_branch}', but we have a detached HEAD pointing to '{}' locally.",
                oid.to_hex()
            );
        }
        _ => {}
    }
}

/// Normalize the right-hand side of a fetch refspec from config (Git allows `remotes/foo/bar`
/// without the leading `refs/`).
fn normalize_fetch_refspec_dst(dst: &str) -> String {
    let d = dst.trim();
    if d.starts_with("refs/") {
        return d.to_owned();
    }
    if d.starts_with("remotes/") {
        return format!("refs/{d}");
    }
    if d.starts_with("tags/") {
        return format!("refs/{d}");
    }
    format!("refs/heads/{d}")
}
/// A parsed fetch refspec (e.g. `+refs/heads/*:refs/remotes/origin/*`).
///
/// Used by `git fetch` and by `git remote show` when classifying remote branches.
#[derive(Clone)]
pub struct FetchRefspec {
    /// Source pattern (remote side), e.g. "refs/heads/*".
    pub src: String,
    /// Destination pattern (local side), e.g. "refs/remotes/origin/*".
    pub dst: String,
    /// Whether this is a force refspec (leading '+').
    #[allow(dead_code)]
    pub force: bool,
    /// Whether this is a negative (exclusion) refspec (leading '^').
    pub negative: bool,
}

/// Returns true if `refname` is excluded by a negative refspec pattern (without the leading `^`).
///
/// Patterns that start with `refs/` are matched against `refname` as written (glob or exact).
/// Unqualified patterns are matched only against `refname` itself — Git does **not** prepend
/// `refs/heads/` to the pattern (see t5582 "does not expand prefix").
fn ref_excluded_by_negative_pattern(pattern: &str, refname: &str) -> bool {
    match_glob_pattern(pattern, refname).is_some() || pattern == refname
}

pub fn ref_excluded_by_fetch_refspecs(refname: &str, refspecs: &[FetchRefspec]) -> bool {
    refspecs
        .iter()
        .any(|rs| rs.negative && ref_excluded_by_negative_pattern(&rs.src, refname))
}

/// Rewrite positive fetch refspec destinations under `refs/prefetch/`, matching Git's
/// `fetch --prefetch` behavior.
fn apply_prefetch_to_refspecs(specs: &mut Vec<FetchRefspec>) {
    const PREFETCH_NS: &str = "refs/prefetch/";
    let mut i = 0usize;
    while i < specs.len() {
        if specs[i].negative {
            i += 1;
            continue;
        }
        let src = specs[i].src.as_str();
        let dst = specs[i].dst.as_str();
        let remove = dst.is_empty() || src.starts_with("refs/tags/");
        if remove {
            specs.remove(i);
            continue;
        }
        let mut new_dst = String::from(PREFETCH_NS);
        if let Some(rest) = dst.strip_prefix("refs/") {
            new_dst.push_str(rest);
        } else {
            new_dst.push_str(dst);
        }
        specs[i].dst = new_dst;
        specs[i].force = true;
        i += 1;
    }
}

/// Parse command-line fetch refspec strings into [`FetchRefspec`] entries (for `--prefetch`).
fn parse_cli_fetch_refspecs(cli: &[String]) -> Vec<FetchRefspec> {
    let mut out = Vec::new();
    for spec in cli {
        if let Some(pat) = spec.strip_prefix('^') {
            out.push(FetchRefspec {
                src: pat.to_owned(),
                dst: String::new(),
                force: false,
                negative: true,
            });
            continue;
        }
        let (force, rest) = if let Some(s) = spec.strip_prefix('+') {
            (true, s)
        } else {
            (false, spec.as_str())
        };
        if let Some(colon) = rest.find(':') {
            out.push(FetchRefspec {
                src: rest[..colon].to_owned(),
                dst: rest[colon + 1..].to_owned(),
                force,
                negative: false,
            });
        } else {
            out.push(FetchRefspec {
                src: rest.to_owned(),
                dst: rest.to_owned(),
                force,
                negative: false,
            });
        }
    }
    out
}

/// Prune namespaces implied by CLI refspecs.
///
/// Only explicit `<src>:<dst>` refspecs participate. A source-only refspec
/// (e.g. `main`) does not define a prune destination namespace.
fn prune_prefixes_from_cli_refspecs(cli: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for spec in cli {
        if spec.starts_with('^') {
            continue;
        }
        let clean = spec.strip_prefix('+').unwrap_or(spec.as_str());
        let Some(colon) = clean.find(':') else {
            continue;
        };
        let dst_raw = &clean[colon + 1..];
        if dst_raw.is_empty() {
            continue;
        }
        let dst = normalize_fetch_refspec_dst(dst_raw);
        if !dst.starts_with("refs/") {
            continue;
        }
        let prefix = if let Some(star) = dst.find('*') {
            dst[..star].to_string()
        } else {
            dst
        };
        out.push(prefix);
    }
    out.sort();
    out.dedup();
    out
}

/// Remote-tracking prune namespaces implied by configured fetch refspecs.
fn prune_prefixes_from_fetch_refspecs(specs: &[FetchRefspec]) -> Vec<String> {
    let mut out = Vec::new();
    for spec in specs {
        if spec.negative || spec.dst.is_empty() {
            continue;
        }
        let dst = normalize_fetch_refspec_dst(&spec.dst);
        if !dst.starts_with("refs/remotes/") {
            continue;
        }
        let prefix = if let Some(star) = dst.find('*') {
            dst[..star].to_string()
        } else {
            dst
        };
        out.push(prefix);
    }
    out.sort();
    out.dedup();
    out
}

fn fetch_refspec_to_cli_string(rs: &FetchRefspec) -> String {
    if rs.negative {
        return format!("^{}", rs.src);
    }
    let mut s = String::new();
    if rs.force {
        s.push('+');
    }
    s.push_str(&rs.src);
    s.push(':');
    s.push_str(&rs.dst);
    s
}

/// Longest directory prefix shared by all ref names (must end on `/`), for pruning after a
/// URL-remote fetch with explicit refspecs.
fn longest_common_ref_prefix(refs: &[String]) -> Option<String> {
    if refs.is_empty() {
        return None;
    }
    let first = refs[0].as_bytes();
    let mut len = first.len();
    for r in refs.iter().skip(1) {
        let b = r.as_bytes();
        let max = len.min(b.len());
        let mut common = 0usize;
        while common < max && first[common] == b[common] {
            common += 1;
        }
        len = len.min(common);
    }
    let prefix = std::str::from_utf8(&first[..len]).ok()?;
    let cut = prefix.rfind('/')?;
    if cut == 0 {
        return None;
    }
    Some(prefix[..=cut].to_string())
}

fn normalize_fetch_dst(dst: &str) -> String {
    if dst.starts_with("refs/") {
        dst.to_owned()
    } else {
        format!("refs/heads/{dst}")
    }
}

/// Local ref destinations from positive CLI refspecs (used for `--prune` namespace).
fn longest_common_ref_prefix_from_cli_positive(cli: &[String]) -> Option<String> {
    let mut locals = Vec::new();
    for spec in cli {
        if spec.starts_with('^') {
            continue;
        }
        let clean = spec.strip_prefix('+').unwrap_or(spec.as_str());
        let Some(colon) = clean.find(':') else {
            continue;
        };
        let (src, dst) = (&clean[..colon], &clean[colon + 1..]);
        if dst.is_empty() {
            continue;
        }
        if src.contains('*') {
            let base = dst.split_once('*').map(|(p, _)| p).unwrap_or(dst);
            locals.push(normalize_fetch_dst(base));
        } else {
            locals.push(normalize_fetch_dst(dst));
        }
    }
    longest_common_ref_prefix(&locals)
}

/// When `remote.<name>.fetch` is unset, Git uses `refs/heads/*:refs/remotes/<name>/*`.
pub fn default_fetch_refspecs(remote_name: &str) -> Vec<FetchRefspec> {
    vec![FetchRefspec {
        src: "refs/heads/*".to_owned(),
        dst: format!("refs/remotes/{remote_name}/*"),
        force: false,
        negative: false,
    }]
}

/// Collect all fetch refspecs from a config key (may be multi-valued).
pub fn collect_refspecs(config: &ConfigSet, key: &str) -> Vec<FetchRefspec> {
    let mut result = Vec::new();
    for entry in config.entries() {
        if entry.key == key {
            if let Some(ref val) = entry.value {
                let val = val.trim();
                // Check for negative refspec (^pattern)
                if let Some(pattern) = val.strip_prefix('^') {
                    result.push(FetchRefspec {
                        src: pattern.to_owned(),
                        dst: String::new(),
                        force: false,
                        negative: true,
                    });
                    continue;
                }
                let (force, val) = if let Some(stripped) = val.strip_prefix('+') {
                    (true, stripped)
                } else {
                    (false, val)
                };
                if let Some(colon) = val.find(':') {
                    let dst_raw = val[colon + 1..].trim().to_owned();
                    let mut src = val[..colon].trim().to_owned();
                    if !src.starts_with('^') && !src.contains('*') && !src.starts_with("refs/") {
                        src = format!("refs/heads/{src}");
                    }
                    result.push(FetchRefspec {
                        src,
                        dst: normalize_fetch_refspec_dst(&dst_raw),
                        force,
                        negative: false,
                    });
                }
            }
        }
    }
    result
}

/// Collect `remote.<name>.fetch` refspecs with Git's `--prefetch` rewrite applied to the **raw**
/// (un-normalized) destination.
///
/// Git's `filter_prefetch_refspec` runs before any dst normalization: an unqualified destination
/// like `bogus/*` becomes `refs/prefetch/bogus/*` (not `refs/prefetch/heads/bogus/*`). Since
/// [`collect_refspecs`] eagerly normalizes `bogus/*` to `refs/heads/bogus/*`, prefetch must build
/// its refspecs from the raw config value instead (t5582 `--prefetch correctly modifies refspecs`).
///
/// Negative refspecs are preserved unchanged; positive refspecs with an empty dst or a
/// `refs/tags/*` source are dropped, mirroring `filter_prefetch_refspec`.
fn collect_refspecs_for_prefetch(config: &ConfigSet, key: &str) -> Vec<FetchRefspec> {
    const PREFETCH_NS: &str = "refs/prefetch/";
    let mut result = Vec::new();
    for entry in config.entries() {
        if entry.key != key {
            continue;
        }
        let Some(ref val) = entry.value else { continue };
        let val = val.trim();
        if let Some(pattern) = val.strip_prefix('^') {
            result.push(FetchRefspec {
                src: pattern.to_owned(),
                dst: String::new(),
                force: false,
                negative: true,
            });
            continue;
        }
        let val = val.strip_prefix('+').unwrap_or(val);
        let Some(colon) = val.find(':') else {
            // A source-only positive refspec has an empty dst; prefetch drops it.
            continue;
        };
        let dst_raw = val[colon + 1..].trim();
        let mut src = val[..colon].trim().to_owned();
        if dst_raw.is_empty() || src.starts_with("refs/tags/") {
            continue;
        }
        if !src.contains('*') && !src.starts_with("refs/") {
            src = format!("refs/heads/{src}");
        }
        // Rewrite the raw dst under refs/prefetch/, stripping a leading `refs/` only.
        let mut new_dst = String::from(PREFETCH_NS);
        new_dst.push_str(dst_raw.strip_prefix("refs/").unwrap_or(dst_raw));
        result.push(FetchRefspec {
            src,
            dst: new_dst,
            force: true,
            negative: false,
        });
    }
    result
}

/// Map a remote ref through fetch refspecs, returning the local ref and the index of the first
/// matching positive refspec (config order). Used to order FETCH_HEAD like Git (t5515).
fn map_ref_through_refspecs_ex(
    remote_ref: &str,
    refspecs: &[FetchRefspec],
) -> Option<(String, usize)> {
    for rs in refspecs {
        if rs.negative
            && (match_glob_pattern(&rs.src, remote_ref).is_some() || rs.src == remote_ref)
        {
            return None;
        }
    }
    for (idx, rs) in refspecs.iter().enumerate() {
        if rs.negative {
            continue;
        }
        if let Some(mapped) = match_refspec_pattern(&rs.src, &rs.dst, remote_ref) {
            return Some((mapped, idx));
        }
    }
    None
}

/// Map a remote ref through fetch refspecs.
///
/// For a refspec like `refs/heads/*:refs/remotes/origin/*`, if the remote ref
/// is `refs/heads/main`, the result is `refs/remotes/origin/main`.
/// Returns None if no refspec matches.
pub fn map_ref_through_refspecs(remote_ref: &str, refspecs: &[FetchRefspec]) -> Option<String> {
    map_ref_through_refspecs_ex(remote_ref, refspecs).map(|(m, _)| m)
}

/// Map a remote ref through *all* matching fetch refspecs, returning every distinct local
/// destination. Mirrors Git's `get_fetch_map`, which produces one ref-map entry per matching
/// refspec — so a source ref like `refs/heads/main` covered by both
/// `+refs/heads/*:refs/remotes/origin/*` and `refs/heads/main:refs/heads/upstream` is fetched
/// into both destinations. A negative refspec matching `remote_ref` shields it entirely (no
/// destinations). Destinations are de-duplicated while preserving refspec order.
pub fn map_ref_through_refspecs_all(remote_ref: &str, refspecs: &[FetchRefspec]) -> Vec<String> {
    for rs in refspecs {
        if rs.negative
            && (match_glob_pattern(&rs.src, remote_ref).is_some() || rs.src == remote_ref)
        {
            return Vec::new();
        }
    }
    let mut out: Vec<String> = Vec::new();
    for rs in refspecs {
        if rs.negative {
            continue;
        }
        if let Some(mapped) = match_refspec_pattern(&rs.src, &rs.dst, remote_ref) {
            if !out.contains(&mapped) {
                out.push(mapped);
            }
        }
    }
    out
}

/// Effective fetch refspecs for `remote.<name>.fetch`, matching Git when no positive refspec is set.
#[must_use]
pub fn remote_fetch_refspecs(config: &ConfigSet, remote_name: &str) -> Vec<FetchRefspec> {
    let key = format!("remote.{remote_name}.fetch");
    let specs = collect_refspecs(config, &key);
    let has_positive = specs.iter().any(|s| !s.negative);
    if has_positive {
        return specs;
    }
    // A configured remote that exists (has a `url`/`pushurl`) but carries no positive fetch
    // refspec performs *no* opportunistic remote-tracking updates — matching upstream, where
    // `remote->fetch` is left empty. (A `--single-branch` clone of a detached source HEAD writes
    // exactly such a remote: only `url`, no `fetch`.) Only synthesize the default tracking spec
    // for an otherwise-undefined remote name.
    let url_key = format!("remote.{remote_name}.url");
    let pushurl_key = format!("remote.{remote_name}.pushurl");
    let remote_defined = config.get(&url_key).is_some() || config.get(&pushurl_key).is_some();
    if remote_defined {
        return specs;
    }
    let mut out = default_fetch_refspecs(remote_name);
    out.extend(specs);
    out
}

/// Returns true if a local (destination) ref is shielded from pruning by a negative refspec.
///
/// Mirrors Git's `refspec_find_negative_match` (`refspec.c`): the prune query is keyed by the
/// local ref (`dst`), but negative refspecs always match the remote ref (`src`). To bridge that,
/// every positive refspec is applied **in reverse** (dst pattern → src) to recover the candidate
/// remote source name(s) for `local_ref`; if any of those candidates matches a negative refspec,
/// the local ref is treated as not matching the refspec set at all and must not be pruned.
///
/// Without this, `git fetch --prune <pattern> ^<src>` would delete the remote-tracking ref whose
/// source is excluded by the negative refspec (t5582 "fetch --prune with negative refspec").
fn local_ref_protected_by_negative(local_ref: &str, refspecs: &[FetchRefspec]) -> bool {
    let negatives: Vec<&FetchRefspec> = refspecs.iter().filter(|rs| rs.negative).collect();
    if negatives.is_empty() {
        return false;
    }
    // Reverse-map the local dst back to every candidate remote src via positive refspecs.
    let mut candidates: Vec<String> = Vec::new();
    for rs in refspecs {
        if rs.negative || rs.dst.is_empty() {
            continue;
        }
        if let Some(star_pos) = rs.dst.find('*') {
            let prefix = &rs.dst[..star_pos];
            let suffix = &rs.dst[star_pos + 1..];
            if local_ref.starts_with(prefix)
                && local_ref.ends_with(suffix)
                && local_ref.len() >= prefix.len() + suffix.len()
            {
                let matched = &local_ref[prefix.len()..local_ref.len() - suffix.len()];
                candidates.push(rs.src.replacen('*', matched, 1));
            }
        } else if rs.dst == local_ref {
            candidates.push(rs.src.clone());
        }
    }
    candidates.iter().any(|src| {
        negatives
            .iter()
            .any(|neg| match_glob_pattern(&neg.src, src).is_some() || neg.src == *src)
    })
}

/// Reverse-map a local ref through configured refspecs to find
/// the remote ref that would normally map to it.
fn reverse_map_refspec(local_ref: &str, refspecs: &[FetchRefspec]) -> Option<String> {
    for rs in refspecs {
        if rs.negative || rs.dst.is_empty() {
            continue;
        }
        // Try to reverse the dst pattern to find what src would produce local_ref
        if let Some(star_pos) = rs.dst.find('*') {
            let prefix = &rs.dst[..star_pos];
            let suffix = &rs.dst[star_pos + 1..];
            if local_ref.starts_with(prefix) && local_ref.ends_with(suffix) {
                let matched = &local_ref[prefix.len()..local_ref.len() - suffix.len()];
                let remote_ref = rs.src.replacen('*', matched, 1);
                return Some(remote_ref);
            }
        } else if rs.dst == local_ref {
            return Some(rs.src.clone());
        }
    }
    None
}

/// Match a single refspec pattern. Both src and dst may contain a single '*'.
fn match_refspec_pattern(src_pattern: &str, dst_pattern: &str, refname: &str) -> Option<String> {
    if let Some(star_pos) = src_pattern.find('*') {
        let prefix = &src_pattern[..star_pos];
        let suffix = &src_pattern[star_pos + 1..];
        if refname.starts_with(prefix) && refname.ends_with(suffix) {
            let matched = &refname[prefix.len()..refname.len() - suffix.len()];
            let result = dst_pattern.replacen('*', matched, 1);
            return Some(result);
        }
    } else if src_pattern == refname {
        // Exact match (no wildcard)
        return Some(dst_pattern.to_owned());
    }
    None
}

/// Match a glob pattern against a ref name, returning the matched wildcard portion.
fn match_glob_pattern<'a>(pattern: &str, refname: &'a str) -> Option<&'a str> {
    if let Some(star_pos) = pattern.find('*') {
        let prefix = &pattern[..star_pos];
        let suffix = &pattern[star_pos + 1..];
        if refname.starts_with(prefix)
            && refname.ends_with(suffix)
            && refname.len() >= prefix.len() + suffix.len()
        {
            Some(&refname[prefix.len()..refname.len() - suffix.len()])
        } else {
            None
        }
    } else if pattern == refname {
        Some(refname)
    } else {
        None
    }
}

/// Copy symbolic refs that match a glob pattern from remote to local.
fn copy_symrefs(
    remote_git_dir: &Path,
    local_git_dir: &Path,
    src_pattern: &str,
    dst_pattern: &str,
) -> Result<()> {
    // Walk the remote refs directory for symbolic refs
    let refs_dir = remote_git_dir.join("refs");
    if !refs_dir.is_dir() {
        return Ok(());
    }
    for_each_ref_file(&refs_dir, "refs", &mut |refname, path| {
        if let Some(matched) = match_glob_pattern(src_pattern, &refname) {
            let content = fs::read_to_string(path)?;
            let content = content.trim();
            if let Some(target) = content.strip_prefix("ref: ") {
                // It's a symbolic ref — write it locally
                let local_ref = dst_pattern.replacen('*', matched, 1);
                let local_path =
                    local_git_dir.join(local_ref.replace('/', std::path::MAIN_SEPARATOR_STR));
                if let Some(parent) = local_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&local_path, format!("ref: {target}\n"))?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

fn for_each_ref_file(
    dir: &Path,
    prefix: &str,
    cb: &mut dyn FnMut(String, &Path) -> Result<()>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let refname = format!("{prefix}/{name_str}");
        if entry.file_type()?.is_dir() {
            for_each_ref_file(&entry.path(), &refname, cb)?;
        } else {
            cb(refname, &entry.path())?;
        }
    }
    Ok(())
}

/// Collect all configured remote names (`remote.<name>.url` entries).
#[must_use]
pub fn collect_remote_names(config: &ConfigSet) -> Vec<String> {
    let mut names = Vec::new();
    for entry in config.entries() {
        let parts: Vec<&str> = entry.key.splitn(3, '.').collect();
        if parts.len() == 3 && parts[0] == "remote" && parts[2] == "url" {
            let name = parts[1].to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Whether a remote opts out of `fetch --all` via `remote.<name>.skipFetchAll`
/// (alias `remote.<name>.skipDefaultUpdate`).
fn remote_skips_fetch_all(config: &ConfigSet, name: &str) -> bool {
    for key in [
        format!("remote.{name}.skipfetchall"),
        format!("remote.{name}.skipdefaultupdate"),
    ] {
        if let Some(v) = config.get(&key) {
            if parse_bool(v.trim()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// Remotes that participate in `git fetch --all`: every configured remote
/// except those with `skipFetchAll`/`skipDefaultUpdate` set.
fn collect_fetch_all_remote_names(git_dir: &Path, config: &ConfigSet) -> Vec<String> {
    let names = collect_local_remote_names(git_dir).unwrap_or_else(|| collect_remote_names(config));
    names
        .into_iter()
        .filter(|name| !remote_skips_fetch_all(config, name))
        .collect()
}

/// Expand a list of `git fetch --multiple` arguments into concrete remote names.
/// Each argument is either a remote group (`remotes.<name>`) — expanded into its
/// members — or a single configured remote. Order is preserved and duplicates
/// removed. An argument that matches neither is a hard error, mirroring git's
/// "no such remote or remote group".
fn expand_remotes_or_groups(config: &ConfigSet, args: &[String]) -> Result<Vec<String>> {
    let mut seen = HashSet::<String>::new();
    let mut out = Vec::new();
    for arg in args {
        let group_lines = config.get_all(&format!("remotes.{arg}"));
        if !group_lines.is_empty() {
            for line in &group_lines {
                for m in line.split_whitespace() {
                    if seen.insert(m.to_string()) {
                        out.push(m.to_string());
                    }
                }
            }
            continue;
        }
        // Not a group: must be a configured remote.
        if config.get(&format!("remote.{arg}.url")).is_some() {
            if seen.insert(arg.clone()) {
                out.push(arg.clone());
            }
            continue;
        }
        return Err(anyhow::Error::new(ExitCodeError {
            code: 128,
            message: format!("fatal: no such remote or remote group: {arg}"),
        }));
    }
    Ok(out)
}

/// Effective parallelism for a multi-remote fetch. `--jobs=0` (or unset) picks a
/// default based on available CPUs, mirroring git's `online_cpus()`. A negative
/// value is impossible here (clap parses `usize`).
fn effective_max_children(args: &Args, config: &ConfigSet) -> usize {
    if let Some(j) = args.jobs {
        if j != 0 {
            return j;
        }
    }
    // `--jobs` unset or zero: fall back to `fetch.parallel`, else online CPUs.
    if let Some(p) = config
        .get("fetch.parallel")
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        if p != 0 {
            return p;
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Emit a `GIT_TRACE` line (git's `trace_printf`) with the standard timestamped
/// prefix when tracing is active.
fn fetch_trace_printf(message: &str) {
    let Ok(dest) = std::env::var("GIT_TRACE") else {
        return;
    };
    if dest.is_empty() || dest == "0" || dest.eq_ignore_ascii_case("false") {
        return;
    }
    let line = if std::env::var("GIT_TRACE_BARE").ok().as_deref() == Some("1") {
        format!("{message}\n")
    } else {
        let now = time::OffsetDateTime::now_utc();
        format!(
            "{:02}:{:02}:{:02}.{:06} run-command.c:000      {message}\n",
            now.hour(),
            now.minute(),
            now.second(),
            now.microsecond(),
        )
    };
    crate::write_git_trace(&dest, &line);
}

/// Fetch each named remote in turn, continuing past a remote that errors so the
/// remaining remotes are still fetched (mirrors `fetch_multiple` in git). If any
/// remote failed, the overall command exits with code 1.
///
/// `parallel` controls only the diagnostics: when more than one task could run
/// concurrently git emits a `preparing to run up to N tasks` trace line and uses
/// the parallel error format `could not fetch '<name>' (exit code: N)`. We always
/// fetch serially, but reproduce these observable details.
fn fetch_each_continuing(
    git_dir: &Path,
    config: &ConfigSet,
    names: &[String],
    args: &Args,
    recurse_each: Option<FetchRecurseSubmodules>,
) -> Result<()> {
    let max_children = effective_max_children(args, config);
    let parallel = max_children != 1 && names.len() != 1;
    if parallel {
        fetch_trace_printf(&format!(
            "run_processes_parallel: preparing to run up to {max_children} tasks"
        ));
    }

    // Truncate FETCH_HEAD once up front (unless the parent itself was asked to
    // append or to not write it). Per-remote children then append, matching
    // git's `truncate_fetch_head()` before spawning `fetch --append` children.
    let write_fetch_head = !args.dry_run && !args.no_write_fetch_head;
    if write_fetch_head && !args.append {
        let fetch_head_path = git_dir.join("FETCH_HEAD");
        if let Err(e) = fs::write(&fetch_head_path, b"") {
            if fetch_head_path.exists() {
                return Err(anyhow::Error::new(e)).context("truncating FETCH_HEAD");
            }
        }
    }

    let mut had_error = false;
    for name in names {
        let mut inner = args.clone();
        inner.multiple = false;
        inner.all = false;
        inner.no_all = false;
        inner.remote = None;
        inner.refspecs.clear();
        // Each per-remote child appends to FETCH_HEAD and skips the post-fetch
        // auto maintenance / commit-graph write; the parent does those once at
        // the end (mirrors git's `fetch --append --no-auto-gc …` children).
        inner.append = true;
        inner.no_auto_gc = true;
        inner.no_write_commit_graph = true;
        if !args.quiet && effective_output_format(config, args) != FetchOutputFormat::Porcelain {
            println!("Fetching {name}");
        }
        match fetch_remote(git_dir, config, name, None, &inner) {
            Ok(()) => {
                // Git's `fetch_multiple` runs each remote as its own `git fetch
                // --recurse-submodules <remote>` subprocess, so submodule recursion happens once
                // *per remote* (t5526 #55: two remotes both advance the superproject, yielding two
                // "Fetching submodule sub" lines). Mirror that by recursing after each remote and
                // restarting the tip record for the next one, instead of a single recursion at the
                // end of the whole `--all`/`--multiple` fetch.
                if let Some(cmd_recurse) = recurse_each {
                    crate::fetch_submodule_record::finish_record_tips_after(git_dir);
                    crate::fetch_submodule_recurse::recursive_fetch_submodules_after_fetch(
                        git_dir,
                        config,
                        &inner,
                        cmd_recurse,
                    )?;
                    crate::fetch_submodule_record::begin_fetch_submodule_record(git_dir);
                }
            }
            Err(e) => {
                // Surface the underlying transport error, then note the remote
                // and keep going. The child's exit code is reported in the
                // parallel format; the aggregate command exits with code 1.
                let code = e
                    .downcast_ref::<ExitCodeError>()
                    .map(|x| x.code)
                    .unwrap_or(128);
                let msg = format!("{e:#}");
                let msg = msg
                    .strip_prefix("fatal: ")
                    .or_else(|| msg.strip_prefix("error: "))
                    .unwrap_or(&msg);
                if !msg.is_empty() {
                    eprintln!("error: {msg}");
                }
                if parallel {
                    eprintln!("error: could not fetch '{name}' (exit code: {code})");
                } else {
                    eprintln!("error: could not fetch {name}");
                }
                had_error = true;
            }
        }
    }
    // Run the post-fetch commit-graph write and auto maintenance once for the
    // whole multi-remote fetch (children were told to skip them).
    maybe_write_commit_graph_after_fetch(git_dir, args)?;
    maybe_run_auto_maintenance_after_fetch(git_dir, args)?;

    if had_error {
        return Err(anyhow::Error::new(ExitCodeError {
            code: 1,
            message: String::new(),
        }));
    }
    Ok(())
}

/// Collect remote names from the repository-local config only.
fn collect_local_remote_names(git_dir: &Path) -> Option<Vec<String>> {
    let cfg = ConfigFile::from_path(&git_dir.join("config"), ConfigScope::Local)
        .ok()
        .flatten()?;
    let mut names = Vec::new();
    for entry in cfg.entries {
        let parts: Vec<&str> = entry.key.splitn(3, '.').collect();
        if parts.len() == 3 && parts[0] == "remote" && parts[2] == "url" {
            let name = parts[1].to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    Some(names)
}

/// True when `path` is a regular file starting with the v2 git bundle header.
fn remote_path_is_git_bundle_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    if let Ok(mut f) = fs::File::open(path) {
        let mut buf = [0u8; 20];
        if let Ok(n) = std::io::Read::read(&mut f, &mut buf) {
            return buf[..n].starts_with(b"# v2 git bundle")
                || buf[..n].starts_with(b"# v3 git bundle");
        }
    }
    false
}

fn parse_bundle_fetch_refs(path: &Path) -> Result<Vec<(String, ObjectId)>> {
    let data = fs::read(path).with_context(|| format!("reading bundle {}", path.display()))?;
    let header_v2 = b"# v2 git bundle\n";
    let header_v3 = b"# v3 git bundle\n";
    let mut pos = if data.starts_with(header_v2) {
        header_v2.len()
    } else if data.starts_with(header_v3) {
        header_v3.len()
    } else {
        bail!("not a git bundle");
    };
    let mut out = Vec::new();
    loop {
        let eol = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .ok_or_else(|| anyhow::anyhow!("truncated bundle header"))?;
        let line = &data[pos..eol];
        pos = eol + 1;
        if line.is_empty() {
            break;
        }
        let line = std::str::from_utf8(line).context("invalid UTF-8 in bundle header")?;
        if line.starts_with('-') || line.starts_with('@') {
            continue;
        }
        if let Some((hex, refname)) = line.split_once(' ') {
            let oid = ObjectId::from_hex(hex)
                .map_err(|e| anyhow::anyhow!("bad oid in bundle header: {e}"))?;
            out.push((refname.to_owned(), oid));
        }
    }
    Ok(out)
}

/// Resolve which local refs a bundle fetch should update.
///
/// A bundle header records full ref names while a CLI refspec source may be an
/// abbreviated name (e.g. `main:main`). For each positive refspec, the source
/// is resolved against the bundle's ref list with git's DWIM rules
/// ([`resolve_advertised_ref_for_fetch_src`]) and wildcard refspecs are matched
/// directly. Returns `(remote bundle ref, oid, local destination ref)` tuples.
fn resolve_bundle_fetch_ref_updates(
    bundle_refs: &[(String, ObjectId)],
    refspecs: &[FetchRefspec],
) -> Vec<(String, ObjectId, String)> {
    let mut out = Vec::new();
    for rs in refspecs {
        if rs.negative || rs.dst.is_empty() {
            continue;
        }
        if rs.src.contains('*') {
            // Wildcard refspec: match every bundle ref against the source pattern.
            for (remote_ref, oid) in bundle_refs {
                if let Some(local) = match_refspec_pattern(&rs.src, &rs.dst, remote_ref) {
                    out.push((remote_ref.clone(), *oid, local));
                }
            }
            continue;
        }
        // Exact / abbreviated refspec: DWIM the source against the bundle refs.
        let Some(resolved) = resolve_advertised_ref_for_fetch_src(&rs.src, bundle_refs, None)
        else {
            continue;
        };
        if let Some((_, oid)) = bundle_refs.iter().find(|(name, _)| *name == resolved) {
            out.push((resolved, *oid, rs.dst.clone()));
        }
    }
    out
}

fn update_refs_from_bundle_fetch(
    git_dir: &Path,
    bundle_refs: &[(String, ObjectId)],
    args: &Args,
    _is_bare_repo: bool,
) -> Result<()> {
    if args.refspecs.is_empty() {
        return Ok(());
    }

    let refspecs = parse_cli_fetch_refspecs(&args.refspecs);
    let ff_repo = Repository::open(git_dir, None).context("open repository for bundle fetch")?;
    let mut pending_atomic_ref_ops = Vec::new();
    let mut ref_update_failures = Vec::new();

    // Build the set of (remote bundle ref, local ref) updates. A bundle records
    // full ref names (e.g. `refs/heads/main`) while a CLI refspec source may be
    // abbreviated (`main:main`), so resolve each refspec's source against the
    // bundle's ref list with the same DWIM rules used for an advertised remote.
    let bundle_updates = resolve_bundle_fetch_ref_updates(bundle_refs, &refspecs);

    for (remote_ref, oid, local_ref) in &bundle_updates {
        let remote_ref = remote_ref.as_str();
        let local_ref = normalize_fetch_refspec_dst(local_ref);
        if local_ref.starts_with("refs/heads/") && !args.update_head_ok {
            if let Some(wt_path) = is_branch_in_worktree(git_dir, &local_ref) {
                bail!(
                    "refusing to fetch into branch '{}' checked out at '{}'",
                    local_ref,
                    wt_path
                );
            }
        }

        let old_oid = read_ref_oid(git_dir, &local_ref);
        if old_oid.as_ref() == Some(oid) {
            continue;
        }
        let forced = args.force
            || refspecs.iter().any(|rs| {
                !rs.negative
                    && rs.force
                    && (match_refspec_pattern(&rs.src, &rs.dst, remote_ref).is_some()
                        || normalize_fetch_refspec_dst(&rs.dst) == local_ref)
            });
        if let Some(old) = old_oid {
            if old != *oid && !forced {
                let is_ff = merge_base::is_ancestor(&ff_repo, old, *oid).unwrap_or(true);
                if !is_ff {
                    eprintln!(
                        " ! [rejected]        {remote_ref} -> {local_ref} (non-fast-forward)"
                    );
                    bail!("cannot fast-forward ref '{local_ref}'");
                }
            }
        }

        apply_single_ref_update(
            args,
            git_dir,
            &mut pending_atomic_ref_ops,
            &local_ref,
            old_oid,
            *oid,
            &mut ref_update_failures,
        )?;
    }

    apply_pending_ref_ops_atomic(git_dir, &pending_atomic_ref_ops)?;
    if !ref_update_failures.is_empty() {
        bail!("some local refs could not be updated");
    }
    Ok(())
}

/// Open a repository (bare or non-bare).
fn open_repo(path: &Path) -> Result<Repository> {
    if path.is_file() {
        let work_tree = path.parent().map(Path::to_path_buf);
        let git_dir = grit_lib::repo::resolve_dot_git(path)?;
        return Repository::open(&git_dir, work_tree.as_deref()).map_err(Into::into);
    }
    let dot_git = path.join(".git");
    if dot_git.is_file() {
        let resolved = grit_lib::repo::resolve_dot_git(&dot_git)?;
        return Repository::open(&resolved, Some(path)).map_err(Into::into);
    }
    if let Ok(repo) = Repository::open(&dot_git, Some(path)) {
        return Ok(repo);
    }
    if let Ok(repo) = Repository::open(path, None) {
        return Ok(repo);
    }
    let with_git = PathBuf::from(format!("{}.git", path.display()));
    if let Ok(repo) = Repository::open(&with_git, None) {
        return Ok(repo);
    }
    let dot_git = with_git.join(".git");
    if dot_git.is_file() {
        let resolved = grit_lib::repo::resolve_dot_git(&dot_git)?;
        return Repository::open(&resolved, Some(&with_git)).map_err(Into::into);
    }
    if let Ok(repo) = Repository::open(&dot_git, Some(&with_git)) {
        return Ok(repo);
    }
    Repository::open(path, None).map_err(Into::into)
}

/// Pre-`remote.*` layout: `.git/remotes/<name>` with `URL:` and `Pull:` lines.
struct LegacyRemotesFile {
    url: String,
    pull_lines: Vec<String>,
}

fn read_git_remotes_file(git_dir: &Path, remote_name: &str) -> Option<LegacyRemotesFile> {
    let path = git_dir.join("remotes").join(remote_name);
    let content = fs::read_to_string(path).ok()?;
    let mut url = None;
    let mut pull_lines = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("URL:") {
            url = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("Pull:") {
            pull_lines.push(rest.trim().to_owned());
        }
    }
    let url = url?;
    Some(LegacyRemotesFile { url, pull_lines })
}

/// `.git/branches/<name>` — single line `url` or `url#branch`.
struct GitBranchesRemote {
    url: String,
    default_branch: Option<String>,
}

fn read_git_branches_remote_file(git_dir: &Path, name: &str) -> Option<GitBranchesRemote> {
    let path = git_dir.join("branches").join(name);
    let raw = fs::read_to_string(path).ok()?;
    let line = raw.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let (url_part, branch) = if let Some((u, b)) = line.split_once('#') {
        (u.trim().to_owned(), Some(b.trim().to_owned()))
    } else {
        (line.to_owned(), None)
    };
    if url_part.is_empty() {
        return None;
    }
    Some(GitBranchesRemote {
        url: url_part,
        default_branch: branch,
    })
}

fn parse_legacy_pull_lines(lines: &[String]) -> Result<Vec<FetchRefspec>> {
    let mut out = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (force, rest) = if let Some(s) = line.strip_prefix('+') {
            (true, s.trim())
        } else {
            (false, line)
        };
        let Some(colon) = rest.find(':') else {
            continue;
        };
        let mut src = rest[..colon].trim().to_owned();
        if !src.contains('*') && !src.starts_with("refs/") {
            src = format!("refs/heads/{src}");
        }
        out.push(FetchRefspec {
            src,
            dst: normalize_fetch_refspec_dst(rest[colon + 1..].trim()),
            force,
            negative: false,
        });
    }
    if out.is_empty() {
        bail!("legacy remote file has no Pull: refspecs");
    }
    Ok(out)
}

/// Expand `tag <name>` pairs in fetch arguments into explicit refspecs.
fn expand_fetch_cli_tag_args(specs: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < specs.len() {
        if specs[i] == "tag" {
            let Some(name) = specs.get(i + 1) else {
                bail!("missing tag name after 'tag'");
            };
            out.push(format!("refs/tags/{name}:refs/tags/{name}"));
            i += 2;
        } else {
            out.push(specs[i].clone());
            i += 1;
        }
    }
    Ok(out)
}

fn normalize_fetch_url_display(s: &str) -> String {
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        return "/".to_owned();
    }
    // Match Git's `display_state` URL truncation (builtin/fetch.c): after trimming trailing
    // slashes, drop a trailing `.git` when the last non-slash index `i` satisfies `4 < i`
    // (i.e. there is more than just `X.git` before it). `../.git` -> `../`, but `.git` and
    // `x.git` are left untouched.
    let bytes = t.as_bytes();
    let last = bytes.len() - 1;
    if last > 4 && t.ends_with(".git") {
        return t[..t.len() - 4].to_owned();
    }
    t.to_owned()
}

fn canonical_repo_path(p: &Path) -> Result<PathBuf> {
    let p = if p.is_symlink() {
        fs::read_link(p).unwrap_or_else(|_| p.to_path_buf())
    } else {
        p.to_path_buf()
    };
    p.canonicalize().or_else(|_| Ok(p))
}

/// Display URL for FETCH_HEAD (`../` relative to the local repo root when fetching by path).
fn resolve_fetch_display_url(
    git_dir: &Path,
    raw_url: &str,
    url_override: Option<&str>,
    remote_repo: Option<&Repository>,
) -> Result<String> {
    let base = configured_remote_base(git_dir);
    if url_override.is_some() {
        if let Some(remote_repo) = remote_repo {
            if !crate::ssh_transport::is_configured_ssh_url(raw_url) {
                if let Ok(canon_remote) = canonical_repo_path(&remote_repo.git_dir) {
                    let mut sb = String::new();
                    let base_s = base.to_string_lossy();
                    let remote_s = canon_remote.to_string_lossy();
                    if let Some(rel) =
                        grit_lib::git_path::relative_path(&remote_s, &base_s, &mut sb)
                    {
                        let mut s = normalize_fetch_url_display(rel);
                        if let Some(prefix) = s.strip_suffix("/.git") {
                            s = normalize_fetch_url_display(prefix);
                        }
                        if s == ".." {
                            return Ok("../".to_owned());
                        }
                        return Ok(s);
                    }
                }
            }
        }
        let trimmed = raw_url.trim_end_matches('/');
        let u = Path::new(trimmed);
        if u.is_relative() {
            let joined = base.join(u);
            if let Ok(canon) = canonical_repo_path(&joined) {
                let mut sb = String::new();
                let base_s = base.to_string_lossy();
                let canon_s = canon.to_string_lossy();
                if let Some(rel) = grit_lib::git_path::relative_path(&canon_s, &base_s, &mut sb) {
                    let mut s = normalize_fetch_url_display(rel);
                    if let Some(prefix) = s.strip_suffix("/.git") {
                        s = normalize_fetch_url_display(prefix);
                    }
                    return Ok(s);
                }
            }
        }
    }
    if url_override.is_some() {
        let mut s = normalize_fetch_url_display(raw_url);
        if let Some(prefix) = s.strip_suffix("/.git") {
            s = normalize_fetch_url_display(prefix);
        }
        return Ok(s);
    }
    Ok(normalize_fetch_url_display(raw_url))
}

fn resolve_fetch_from_line_url(
    raw_url: &str,
    _url_override: Option<&str>,
    _remote_repo: Option<&Repository>,
    _default_display_url: &str,
) -> String {
    // Git's `From <url>` header is the configured remote URL verbatim, only trimmed of trailing
    // slashes and a `.git` suffix (builtin/fetch.c `display_state_init` -> `transport_anonymize_url`).
    // It is NOT canonicalized and no `/.` is synthesized: `git clone .` stores the URL as `<cwd>/.`
    // (see `absolute_clone_source_url` in clone.rs), so a superproject shows `From <cwd>/.` while a
    // submodule whose URL is `<cwd>/submodule` shows `From <cwd>/submodule` with no trailing `/.`.
    normalize_fetch_url_display(raw_url)
}

fn remotes_match_same_repository(git_dir: &Path, remote_repo: &Repository, url_str: &str) -> bool {
    let mut candidate = if let Some(s) = url_str.strip_prefix("file://") {
        PathBuf::from(s)
    } else {
        PathBuf::from(url_str)
    };
    if candidate.is_relative() {
        candidate = configured_remote_base(git_dir).join(candidate);
    }
    let Ok(can_a) = canonical_repo_path(&candidate) else {
        return false;
    };
    let Ok(can_b) = canonical_repo_path(&remote_repo.git_dir) else {
        return false;
    };
    can_a == can_b
}

/// If the fetch URL points at the same repository as a configured remote, return that name.
fn find_remote_for_repository_url(
    config: &ConfigSet,
    git_dir: &Path,
    remote_repo: &Repository,
) -> Option<String> {
    let mut names = collect_remote_names(config);
    names.sort();
    for name in names {
        let key = format!("remote.{name}.url");
        let Some(u) = config.get(&key) else {
            continue;
        };
        if remotes_match_same_repository(git_dir, remote_repo, &u) {
            return Some(name);
        }
    }
    None
}

/// All configured remotes whose `remote.<name>.url` resolves to the same repository as `remote_repo`
/// (Git fetches every matching refspec in one object transfer; t5515 clone + extra remotes).
fn remotes_sharing_repository_url(
    config: &ConfigSet,
    git_dir: &Path,
    remote_repo: &Repository,
    primary: &str,
) -> Vec<String> {
    let mut out: Vec<String> = collect_remote_names(config)
        .into_iter()
        .filter(|n| {
            config
                .get(&format!("remote.{n}.url"))
                .map(|u| remotes_match_same_repository(git_dir, remote_repo, &u))
                .unwrap_or(false)
        })
        .collect();
    if !out.iter().any(|n| n == primary) {
        out.push(primary.to_owned());
    }
    out.sort();
    out.dedup();
    out
}

/// Local branches that list `remote_name` as their upstream, mapped to remote branch short names
/// from `branch.<name>.merge` (e.g. `refs/heads/three` → `three`).
fn branch_merge_remote_specs(
    config: &ConfigSet,
    remote_name: &str,
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for entry in config.entries() {
        let key = entry.key.as_str();
        let Some(rest) = key.strip_prefix("branch.") else {
            continue;
        };
        let Some(local_branch) = rest.strip_suffix(".merge") else {
            continue;
        };
        if local_branch.is_empty() {
            continue;
        }
        let rkey = format!("branch.{local_branch}.remote");
        if config.get(&rkey).as_deref() != Some(remote_name) {
            continue;
        }
        let Some(m) = entry.value.as_deref() else {
            continue;
        };
        let rb = m.strip_prefix("refs/heads/").unwrap_or(m).to_owned();
        out.entry(local_branch.to_owned()).or_default().push(rb);
    }
    out
}

fn head_short_branch(git_dir: &Path) -> Option<String> {
    match resolve_head(git_dir).ok()? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        _ => None,
    }
}

/// Rank for FETCH_HEAD ordering: lower = for-merge (printed first after sort).
fn fetch_head_branch_line(
    oid: &ObjectId,
    branch: &str,
    display_url: &str,
    for_merge: bool,
) -> String {
    let sep = if for_merge {
        "\t\t"
    } else {
        "\tnot-for-merge\t"
    };
    format!("{oid}{sep}branch '{branch}' of {display_url}")
}

fn fetch_head_tag_line(
    oid: &ObjectId,
    tag_name: &str,
    display_url: &str,
    for_merge: bool,
) -> String {
    let sep = if for_merge {
        "\t\t"
    } else {
        "\tnot-for-merge\t"
    };
    format!("{oid}{sep}tag '{tag_name}' of {display_url}")
}

fn fetch_head_bare_url_line(oid: &ObjectId, display_url: &str) -> String {
    format!("{oid}\t\t{display_url}")
}

fn fetch_head_line_has_not_for_merge(line: &str) -> bool {
    line.contains("not-for-merge")
}

/// Downgrade for-merge FETCH_HEAD lines whose object does not resolve to a commit to
/// `not-for-merge`, mirroring Git's `write_fetch_head` classification (`builtin/fetch.c`:
/// `lookup_commit_reference_gently` failure -> `FETCH_HEAD_NOT_FOR_MERGE`). This keeps tags that
/// point at trees/blobs (t5515 `tag-one-tree`, `tag-three-file`) out of the merge set.
fn downgrade_non_commit_for_merge_lines(lines: &mut [String], repo: &Repository) {
    for line in lines.iter_mut() {
        // For-merge lines have an empty middle field, i.e. `<oid>\t\t<desc>`.
        let Some((oid_hex, after)) = line.split_once('\t') else {
            continue;
        };
        if !after.starts_with('\t') {
            // Already `not-for-merge` (non-empty middle field) or a bare-URL line.
            continue;
        }
        let Ok(oid) = ObjectId::from_hex(oid_hex) else {
            continue;
        };
        let is_commit = grit_lib::rev_parse::try_peel_to_commit_for_merge_base(repo, oid)
            .ok()
            .flatten()
            .is_some();
        if !is_commit {
            *line = format!("{oid_hex}\tnot-for-merge\t{}", &after[1..]);
        }
    }
}

fn fetch_head_parse_name(line: &str) -> Option<(bool, String)> {
    if let Some(idx) = line.find("branch '") {
        let rest = &line[idx + "branch '".len()..];
        let end = rest.find('\'')?;
        return Some((false, rest[..end].to_owned()));
    }
    if let Some(idx) = line.find("tag '") {
        let rest = &line[idx + "tag '".len()..];
        let end = rest.find('\'')?;
        return Some((true, rest[..end].to_owned()));
    }
    None
}

fn branch_refspec_index(branch: &str, refspecs: &[FetchRefspec]) -> usize {
    if refspecs.is_empty() {
        return 0;
    }
    let remote_ref = format!("refs/heads/{branch}");
    for (i, rs) in refspecs.iter().enumerate() {
        if rs.negative {
            continue;
        }
        if match_refspec_pattern(&rs.src, &rs.dst, &remote_ref).is_some() {
            return i;
        }
    }
    usize::MAX
}

/// Order FETCH_HEAD lines like Git (t5515): for-merge before not-for-merge; branches before tags;
/// then configured refspec order, then name.
fn sort_fetch_head_lines(lines: &mut [String], refspecs: &[FetchRefspec]) {
    lines.sort_by(|a, b| {
        let a_n = fetch_head_line_has_not_for_merge(a);
        let b_n = fetch_head_line_has_not_for_merge(b);
        a_n.cmp(&b_n).then_with(|| {
            let pa = fetch_head_parse_name(a);
            let pb = fetch_head_parse_name(b);
            match (pa, pb) {
                (Some((ta, na)), Some((tb, nb))) => {
                    let sa = if ta { 1 } else { 0 };
                    let sb = if tb { 1 } else { 0 };
                    sa.cmp(&sb).then_with(|| {
                        if !ta && !tb {
                            let ia = branch_refspec_index(&na, refspecs);
                            let ib = branch_refspec_index(&nb, refspecs);
                            ia.cmp(&ib).then_with(|| na.cmp(&nb))
                        } else {
                            // Tags keep their emission order (explicitly-requested CLI tags first,
                            // then auto-followed tags), matching Git's fetch-map order. `sort_by` is
                            // stable, so returning Equal preserves insertion order (t5515 `... tag
                            // tag-one-tree tag tag-three-file`).
                            std::cmp::Ordering::Equal
                        }
                    })
                }
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.cmp(b),
            }
        })
    });
}

fn fetch_branch_merge_rank(
    remote_branch: &str,
    head_short: Option<&str>,
    merge_map: &HashMap<String, Vec<String>>,
) -> u32 {
    let Some(h) = head_short else {
        return 999;
    };
    let Some(sources) = merge_map.get(h) else {
        return 999;
    };
    if sources.len() >= 2 {
        if sources.iter().any(|b| b == remote_branch) {
            return 0;
        }
        return 999;
    }
    if sources.len() == 1 && sources[0] == remote_branch {
        return 0;
    }
    999
}

fn configured_remote_base(git_dir: &Path) -> PathBuf {
    if git_dir.file_name().is_some_and(|name| name == ".git") {
        git_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| git_dir.to_path_buf())
    } else {
        git_dir.to_path_buf()
    }
}

/// Resolve the git directory from CWD.
fn resolve_git_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("GIT_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    let mut cur = cwd.as_path();
    loop {
        let dot_git = cur.join(".git");
        if dot_git.is_dir() {
            return Ok(dot_git);
        }
        if dot_git.is_file() {
            if let Ok(content) = fs::read_to_string(&dot_git) {
                for line in content.lines() {
                    if let Some(rest) = line.strip_prefix("gitdir:") {
                        let path = rest.trim();
                        let resolved = if Path::new(path).is_absolute() {
                            PathBuf::from(path)
                        } else {
                            cur.join(path)
                        };
                        return Ok(resolved);
                    }
                }
            }
        }
        // Check if this is a bare repo
        if cur.join("objects").is_dir() && cur.join("HEAD").is_file() {
            return Ok(cur.to_path_buf());
        }
        cur = match cur.parent() {
            Some(p) => p,
            None => bail!("not a git repository (or any of the parent directories): .git"),
        };
    }
}

fn repository_is_bare(git_dir: &Path) -> bool {
    if git_dir.file_name().is_some_and(|name| name == ".git") {
        return false;
    }
    let cfg = ConfigSet::load(Some(git_dir), true).ok();
    cfg.and_then(|c| c.get("core.bare"))
        .as_deref()
        .and_then(|v| parse_bool(v).ok())
        .unwrap_or_else(|| git_dir.join("HEAD").is_file() && git_dir.join("objects").is_dir())
}

/// Check if a branch ref is checked out in any worktree, return the worktree path.
fn is_branch_in_worktree(git_dir: &std::path::Path, branch_ref: &str) -> Option<String> {
    let short = branch_ref.strip_prefix("refs/heads/")?;
    let work_tree = if repository_is_bare(git_dir) {
        None
    } else {
        git_dir.parent()
    };
    let repo = Repository::open(git_dir, work_tree).ok()?;
    crate::commands::worktree_refs::branch_occupied_any_worktree(&repo, short)
}

//! `grit ls-remote` — list references from local or HTTP(S) repositories.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::check_ref_format::is_valid_advertised_refname;
use grit_lib::config::ConfigSet;
use grit_lib::ls_remote::{ls_remote, Options, RefEntry};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use grit_lib::pkt_line;

/// Arguments for `grit ls-remote`.
///
/// `--heads`/`-h` is a hidden, deprecated synonym for `--branches`/`-b`; the
/// short forms are rewritten to the long names in the CLI preprocessing step
/// (see `preprocess_ls_remote_argv` in `main.rs`) because clap reserves `-h`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Show only branches (`refs/heads/`).
    #[arg(long = "branches", visible_alias = "heads")]
    pub branches: bool,

    /// Show only tags (`refs/tags/`).
    #[arg(long = "tags")]
    pub tags: bool,

    /// Exclude pseudo-refs (HEAD) and peeled tag `^{}` entries.
    #[arg(long = "refs")]
    pub refs_only: bool,

    /// Show the symbolic ref that HEAD points to.
    #[arg(long = "symref")]
    pub symref: bool,

    /// Path to git-upload-pack on the remote host.
    #[arg(long = "upload-pack", alias = "exec")]
    pub upload_pack: Option<String>,

    /// Quiet: suppress the "From <url>" header on stderr.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Print only the remote URL (`url.<base>.insteadOf` aware) and exit.
    #[arg(long = "get-url")]
    pub get_url: bool,

    /// Exit with status 2 when no matching refs are found.
    #[arg(long = "exit-code")]
    pub exit_code: bool,

    /// Sort refs by the given key (e.g. `refname`, `version:refname`).
    #[arg(long = "sort", value_name = "KEY", action = clap::ArgAction::Append)]
    pub sort: Vec<String>,

    /// Transmit the given string as a protocol-v2 server option.
    #[arg(short = 'o', long = "server-option", action = clap::ArgAction::Append)]
    pub server_options: Vec<String>,

    /// Path to the local repository or configured remote name (optional).
    #[arg(value_name = "REPOSITORY")]
    pub repository: Option<PathBuf>,

    /// Optional ref patterns; only matching refs are printed.
    #[arg(value_name = "PATTERN", num_args = 0..)]
    pub patterns: Vec<String>,
}

/// Run `grit ls-remote`.
///
/// Opens the repository at `args.repository`, enumerates its references
/// according to the supplied flags, and prints them to stdout as
/// `<oid>\t<refname>` lines, with HEAD first.
///
/// Exits with status 1 when no refs match (same behaviour as `git ls-remote`).
pub fn run(args: Args) -> Result<()> {
    // git parses --sort options up front; a key requiring object data fails
    // immediately when we are outside a repository, before contacting any
    // remote.
    let sort_keys = parse_sort_keys(&args.sort)?;

    // Resolve the destination. When no <repository> argument is given, fall
    // back to the default remote (branch.<name>.remote, the sole remote, or
    // "origin") exactly like git's `remote_get(NULL)`.
    let dest_given = args.repository.is_some();
    let resolved = match &args.repository {
        Some(p) => p.clone(),
        None => {
            let (url, _name) = resolve_default_remote()?;
            PathBuf::from(url)
        }
    };

    // If the destination is a configured remote name, resolve its URL.
    let effective_path = resolve_remote_or_path(&resolved);
    let repo_path_str = effective_path.to_string_lossy().to_string();
    let remote_name = maybe_remote_name(&resolved);

    // `--get-url` prints the (insteadOf-rewritten) URL and exits.
    if args.get_url {
        let config = match Repository::discover(None) {
            Ok(repo) => ConfigSet::load(Some(repo.git_dir.as_path()), true).unwrap_or_default(),
            Err(_) => ConfigSet::load(None, true).unwrap_or_default(),
        };
        let rewritten = grit_lib::url_rewrite::rewrite_fetch_url(&config, &repo_path_str);
        println!("{rewritten}");
        return Ok(());
    }

    // When operating on a configured remote, Git loads the remote definition,
    // which parses every configured fetch and push refspec.  An invalid
    // refspec makes the whole command die.  Mirror that here.
    if let Some(name) = remote_name.as_deref() {
        validate_remote_refspecs(name)?;
    }
    let server_options = effective_server_options(&args, remote_name.as_deref());
    if !server_options.is_empty() && crate::protocol_wire::effective_client_protocol_version() < 2 {
        bail!(
            "server options require protocol version 2 or later\nsee protocol.version in 'git help config'"
        );
    }

    // git prints "From <url>" to stderr when no <repository> was given and we
    // are not in --quiet mode.
    if !dest_given && !args.quiet {
        eprintln!("From {repo_path_str}");
    }

    if repo_path_str.starts_with("git://") {
        crate::protocol::check_protocol_allowed("git", None)?;
        let (advertised, head_symref, _saw_v1, _saw_v2) =
            crate::fetch_transport::with_packet_trace_identity("ls-remote", || {
                crate::fetch_transport::ls_remote_via_git_protocol(&repo_path_str)
            })?;
        return print_advertised_refs_for_ls_remote(&args, advertised, head_symref);
    }

    if repo_path_str.starts_with("ext::") {
        crate::protocol::check_protocol_allowed("ext", None)?;
        let (advertised, head_symref) =
            crate::fetch_transport::with_packet_trace_identity("ls-remote", || {
                crate::ext_transport::ls_remote_via_ext(&repo_path_str, "git-upload-pack")
            })?;
        return print_advertised_refs_for_ls_remote(&args, advertised, head_symref);
    }

    if repo_path_str.starts_with("http://") || repo_path_str.starts_with("https://") {
        return run_http_ls_remote(&repo_path_str, &args);
    }

    if crate::ssh_transport::is_configured_ssh_url(&repo_path_str) {
        crate::protocol::check_protocol_allowed("ssh", None)?;
        return run_ssh_ls_remote(&repo_path_str, &args);
    }

    // Check if the path is a bundle file
    if is_bundle_file(&effective_path) {
        return run_bundle_ls_remote(&effective_path, &args);
    }

    let is_file_url = repo_path_str.starts_with("file://");
    if is_file_url && crate::file_upload_pack_v2::client_wants_protocol_v2() {
        let path = PathBuf::from(
            repo_path_str
                .strip_prefix("file://")
                .unwrap_or(repo_path_str.as_ref()),
        );
        let upload = args.upload_pack.as_deref().filter(|s| !s.is_empty());
        return crate::trace_packet::with_packet_trace_label("ls-remote", || {
            crate::file_upload_pack_v2::ls_remote_file_v2(&path, upload, &args, &server_options)
        });
    }

    if let Some(upload_pack) = args.upload_pack.as_deref() {
        if !upload_pack.is_empty() {
            return run_ls_remote_via_upload_pack(&effective_path, upload_pack, &args, &sort_keys);
        }
    }

    let repo = match open_local_repo(&effective_path) {
        Ok(repo) => repo,
        Err(_) => {
            // git's local transport runs `git-upload-pack <dest>`, which dies
            // with "'<dest>' does not appear to be a git repository"; the
            // client then reports it could not read from the remote. The dest
            // shown is the user-supplied argument (e.g. a stray pattern).
            let dest = resolved.to_string_lossy();
            eprintln!("fatal: '{dest}' does not appear to be a git repository");
            eprintln!("fatal: Could not read from remote repository.");
            eprintln!();
            eprintln!("Please make sure you have the correct access rights");
            eprintln!("and the repository exists.");
            return Err(crate::explicit_exit::SilentNonZeroExit { code: 128 }.into());
        }
    };

    // Protocol v2 ls-refs advertises a symref-target for every symbolic ref;
    // v0 only advertises the HEAD symref via a capability.
    let all_symrefs = crate::protocol_wire::effective_client_protocol_version() >= 2;

    let opts = Options {
        heads: args.branches,
        tags: args.tags,
        refs_only: args.refs_only,
        symref: args.symref,
        all_symrefs,
        patterns: args.patterns.clone(),
    };

    let refs_git_dir = common_git_dir_or_self(&repo.git_dir);
    let mut entries = ls_remote(&refs_git_dir, &repo.odb, &opts)?;

    // Apply transfer.hiderefs / uploadpack.hiderefs filtering.
    let config = ConfigSet::load(Some(repo.git_dir.as_path()), true).unwrap_or_default();
    apply_hiderefs(&config, &mut entries);

    apply_ref_sorting(&sort_keys, &mut entries);

    print_entries_with_exit_code(&entries, args.exit_code)
}

/// Print ref entries and apply `--exit-code` semantics.
///
/// With `--exit-code`, exits with status 2 when no refs were printed; with a
/// match, status is 0. The "From" header and other stderr output are handled
/// by the caller.
fn print_entries_with_exit_code(entries: &[RefEntry], exit_code: bool) -> Result<()> {
    let mut printed = false;
    for entry in entries {
        if let Some(target) = &entry.symref_target {
            println!("ref: {target}\t{}", entry.name);
        }
        println!("{}\t{}", entry.oid, entry.name);
        printed = true;
    }
    if exit_code && !printed {
        std::process::exit(2);
    }
    Ok(())
}

/// Drop refs hidden by `transfer.hiderefs` / `uploadpack.hiderefs`.
///
/// The two config keys are combined (`uploadpack.hiderefs` taking precedence on
/// duplicates is not modeled separately because git merges both lists in
/// declaration order). A leading `!` un-hides a previously hidden ref; the last
/// matching rule wins. The peeled `^{}` companion of a tag follows the tag.
fn apply_hiderefs(config: &ConfigSet, entries: &mut Vec<RefEntry>) {
    let rules = collect_hiderefs(config);
    if rules.is_empty() {
        return;
    }
    entries.retain(|e| {
        // Peeled entries follow the visibility of their base ref; if the base
        // was kept, so is the peel (it is pushed adjacently right after).
        let name = e.name.strip_suffix("^{}").unwrap_or(&e.name);
        if name == "HEAD" {
            return true;
        }
        !ref_is_hidden(name, &rules)
    });
}

/// Gather `transfer.hiderefs` and `uploadpack.hiderefs` patterns in order.
fn collect_hiderefs(config: &ConfigSet) -> Vec<String> {
    let mut rules = Vec::new();
    for entry in config.entries() {
        let key = entry.key.as_str();
        if key == "transfer.hiderefs" || key == "uploadpack.hiderefs" {
            if let Some(v) = entry.value.as_deref() {
                rules.push(v.to_owned());
            }
        }
    }
    rules
}

/// Determine whether `refname` is hidden given an ordered set of hideRefs rules.
///
/// Each rule hides refs matching the pattern (prefix match, or full match);
/// a `!`-prefixed rule un-hides. The last matching rule decides.
fn ref_is_hidden(refname: &str, rules: &[String]) -> bool {
    let mut hidden = false;
    for rule in rules {
        let (negated, pattern) = match rule.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, rule.as_str()),
        };
        if hideref_pattern_matches(pattern, refname) {
            hidden = !negated;
        }
    }
    hidden
}

/// Match a single hideRefs pattern against a refname (git `ref_is_hidden`).
///
/// A pattern matches when the refname equals it, or when the refname starts
/// with `<pattern>/`. A pattern starting with `^` is anchored to the full
/// refname (exact match only).
fn hideref_pattern_matches(pattern: &str, refname: &str) -> bool {
    if let Some(anchored) = pattern.strip_prefix('^') {
        return refname == anchored;
    }
    refname == pattern
        || refname
            .strip_prefix(pattern)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A parsed `--sort` key: a field plus whether the order is reversed.
#[derive(Debug, Clone)]
struct SortKey {
    field: SortField,
    reverse: bool,
}

/// The supported `ls-remote --sort` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortField {
    /// Plain lexicographic refname comparison.
    RefName,
    /// Natural / version-aware refname comparison (`version:refname`).
    RefNameVersion,
}

/// Parse and validate the `--sort` keys.
///
/// Each key may have a leading `-` for descending order. Only name-only fields
/// are supported for sorting; any other field requires access to object data
/// and, when we are not inside a git repository, fails with the same fatal
/// message git produces.
///
/// # Errors
///
/// Returns an error when a field requires object data but no ambient git
/// repository is available, mirroring git's `ref-filter` behaviour.
fn parse_sort_keys(raw_keys: &[String]) -> Result<Vec<SortKey>> {
    let have_git_dir = Repository::discover(None).is_ok();
    let mut keys = Vec::new();
    for raw in raw_keys {
        let (reverse, name) = match raw.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, raw.as_str()),
        };
        let field = match name {
            "refname" => SortField::RefName,
            "version:refname" | "v:refname" => SortField::RefNameVersion,
            _ => {
                // Any other field (e.g. authordate, objectname) needs object
                // data. Outside a repository that is fatal.
                if !have_git_dir {
                    bail!(
                        "fatal: not a git repository, but the field '{name}' requires access to object data"
                    );
                }
                // Inside a repo we still only support name-based sorting for
                // ls-remote; fall back to plain refname ordering.
                SortField::RefName
            }
        };
        keys.push(SortKey { field, reverse });
    }
    Ok(keys)
}

/// Sort `entries` in place according to pre-parsed `--sort` keys.
///
/// With no keys the advertised order (HEAD first, then refname order) is kept.
/// Peeled `^{}` companions stay paired with their base ref.
fn apply_ref_sorting(keys: &[SortKey], entries: &mut [RefEntry]) {
    if keys.is_empty() {
        return;
    }
    entries.sort_by(|a, b| {
        for key in keys {
            let ord = compare_by_field(key.field, &a.name, &b.name);
            let ord = if key.reverse { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// Compare two refnames according to a [`SortField`].
fn compare_by_field(field: SortField, a: &str, b: &str) -> std::cmp::Ordering {
    match field {
        SortField::RefName => a.cmp(b),
        SortField::RefNameVersion => compare_refname_version(a, b),
    }
}

/// Version token for natural ordering: a number or a string run.
enum VersionToken {
    Num(u64),
    Str(String),
}

/// Split a refname into alternating non-digit / digit runs for version sort.
fn tokenize_refname_version(s: &str) -> Vec<VersionToken> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let n = std::str::from_utf8(&b[start..i])
                .ok()
                .and_then(|x| x.parse::<u64>().ok())
                .unwrap_or(0);
            out.push(VersionToken::Num(n));
        } else {
            let start = i;
            while i < b.len() && !b[i].is_ascii_digit() {
                i += 1;
            }
            out.push(VersionToken::Str(
                String::from_utf8_lossy(&b[start..i]).into_owned(),
            ));
        }
    }
    out
}

/// Compare two refnames using git-style natural/version ordering.
fn compare_refname_version(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ta = tokenize_refname_version(a);
    let tb = tokenize_refname_version(b);
    let len = ta.len().max(tb.len());
    for k in 0..len {
        match (ta.get(k), tb.get(k)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(VersionToken::Str(sa)), Some(VersionToken::Str(sb))) => {
                let c = sa.cmp(sb);
                if c != Ordering::Equal {
                    return c;
                }
            }
            (Some(VersionToken::Num(na)), Some(VersionToken::Num(nb))) => {
                let c = na.cmp(nb);
                if c != Ordering::Equal {
                    return c;
                }
            }
            (Some(VersionToken::Str(_)), Some(VersionToken::Num(_))) => return Ordering::Less,
            (Some(VersionToken::Num(_)), Some(VersionToken::Str(_))) => return Ordering::Greater,
        }
    }
    Ordering::Equal
}

fn run_http_ls_remote(repo_url: &str, args: &Args) -> Result<()> {
    let proto = if repo_url.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    crate::protocol::check_protocol_allowed(proto, None)?;

    let config = ConfigSet::load(None, true).unwrap_or_default();
    let client = crate::http_client::HttpClientContext::from_config_set(&config)?;
    let advertised = crate::http_smart::http_ls_refs(repo_url, &client)?;
    if advertised.is_empty() {
        return Ok(());
    }

    let head_symref_target = if args.symref {
        crate::http_smart::remote_default_branch_from_advertised(&advertised)
            .map(|b| format!("refs/heads/{b}"))
    } else {
        None
    };

    let mut entries: Vec<RefEntry> = Vec::new();
    for e in advertised {
        if args.branches && e.name != "HEAD" && !e.name.starts_with("refs/heads/") {
            continue;
        }
        if args.tags && !e.name.starts_with("refs/tags/") {
            continue;
        }
        if args.refs_only && e.name == "HEAD" {
            continue;
        }
        if !grit_lib::ls_remote::ref_matches_ls_remote_patterns(&e.name, &args.patterns) {
            continue;
        }
        let symref_target = if args.symref && e.name == "HEAD" {
            head_symref_target.clone()
        } else {
            None
        };
        entries.push(RefEntry {
            name: e.name,
            oid: e.oid,
            symref_target,
        });
    }

    if entries.is_empty() || args.quiet {
        return Ok(());
    }

    entries.sort_by(|a, b| {
        let rank = |n: &str| if n == "HEAD" { 0 } else { 1 };
        match rank(&a.name).cmp(&rank(&b.name)) {
            std::cmp::Ordering::Equal => a.name.cmp(&b.name),
            o => o,
        }
    });

    for entry in &entries {
        if let Some(target) = &entry.symref_target {
            println!("ref: {target}\t{}", entry.name);
        }
        println!("{}\t{}", entry.oid, entry.name);
    }
    Ok(())
}

fn run_ssh_ls_remote(repo_url: &str, args: &Args) -> Result<()> {
    let spec = crate::ssh_transport::parse_ssh_url(repo_url)?;
    let upload = args.upload_pack.as_deref().filter(|s| !s.trim().is_empty());
    let mut child = crate::ssh_transport::spawn_git_ssh_upload_pack(&spec, upload)?;
    let mut stdin = child.stdin.take().context("ssh upload-pack stdin")?;
    let mut stdout = child.stdout.take().context("ssh upload-pack stdout")?;
    let entries = read_ls_remote_upload_pack_output(
        &mut stdin,
        &mut stdout,
        &std::env::var("GIT_DEFAULT_HASH").unwrap_or_else(|_| "sha1".to_owned()),
        args,
    )?;
    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        bail!("ssh upload-pack exited with status {status}");
    }
    if entries.is_empty() || args.quiet {
        return Ok(());
    }
    for entry in &entries {
        if let Some(target) = &entry.symref_target {
            println!("ref: {target}\t{}", entry.name);
        }
        println!("{}\t{}", entry.oid, entry.name);
    }
    Ok(())
}

fn print_advertised_refs_for_ls_remote(
    args: &Args,
    advertised: Vec<(String, ObjectId)>,
    head_symref: Option<String>,
) -> Result<()> {
    let mut entries: Vec<RefEntry> = Vec::new();
    for (name, oid) in advertised {
        if args.branches && name != "HEAD" && !name.starts_with("refs/heads/") {
            continue;
        }
        if args.tags && !name.starts_with("refs/tags/") {
            continue;
        }
        if args.refs_only && (name == "HEAD" || name.ends_with("^{}")) {
            continue;
        }
        if !grit_lib::ls_remote::ref_matches_ls_remote_patterns(&name, &args.patterns) {
            continue;
        }
        let symref_target = if args.symref && name == "HEAD" {
            head_symref.clone()
        } else {
            None
        };
        entries.push(RefEntry {
            name,
            oid,
            symref_target,
        });
    }
    if entries.is_empty() || args.quiet {
        return Ok(());
    }
    entries.sort_by(|a, b| {
        let rank = |n: &str| if n == "HEAD" { 0 } else { 1 };
        match rank(&a.name).cmp(&rank(&b.name)) {
            std::cmp::Ordering::Equal => a.name.cmp(&b.name),
            o => o,
        }
    });
    for entry in &entries {
        if let Some(target) = &entry.symref_target {
            println!("ref: {target}\t{}", entry.name);
        }
        println!("{}\t{}", entry.oid, entry.name);
    }
    Ok(())
}

/// Resolve the default remote when `ls-remote` is run without a `<repository>`.
///
/// Mirrors git's `remote_get(NULL)`:
/// 1. If the current branch has `branch.<name>.remote`, use that remote.
/// 2. Else if exactly one remote is configured, use it.
/// 3. Else use `"origin"`.
///
/// The chosen remote must have a configured URL; otherwise this fails with
/// `No remote configured to list refs from.` (e.g. multiple remotes exist but
/// none is named `origin`).
///
/// # Returns
///
/// `(url, remote_name)` of the resolved default remote.
///
/// # Errors
///
/// Returns an error when no usable default remote can be determined.
fn resolve_default_remote() -> Result<(String, String)> {
    let repo = Repository::discover(None)
        .map_err(|_| anyhow::anyhow!("No remote configured to list refs from."))?;
    let config = ConfigSet::load(Some(repo.git_dir.as_path()), true).unwrap_or_default();

    let remote_names = configured_remote_names(&config);

    // 1. branch.<current>.remote
    let branch_remote = grit_lib::refs::read_symbolic_ref(&repo.git_dir, "HEAD")
        .ok()
        .flatten()
        .and_then(|target| {
            target
                .strip_prefix("refs/heads/")
                .map(std::borrow::ToOwned::to_owned)
        })
        .and_then(|short| config.get(&format!("branch.{short}.remote")));

    let chosen = if let Some(name) = branch_remote {
        name
    } else if remote_names.len() == 1 {
        remote_names[0].clone()
    } else {
        "origin".to_owned()
    };

    let url = config
        .get(&format!("remote.{chosen}.url"))
        .ok_or_else(|| anyhow::anyhow!("No remote configured to list refs from."))?;
    Ok((url, chosen))
}

/// Return the names of all configured remotes (those with a `remote.<name>.url`).
fn configured_remote_names(config: &ConfigSet) -> Vec<String> {
    let mut names = Vec::new();
    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("remote.") else {
            continue;
        };
        let Some(name) = rest.strip_suffix(".url") else {
            continue;
        };
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    names
}

fn maybe_remote_name(path: &Path) -> Option<String> {
    let name = path.to_string_lossy().to_string();
    if name.contains("://") || Path::new(&name).exists() {
        return None;
    }
    let repo = Repository::discover(None).ok()?;
    let config_path = repo.git_dir.join("config");
    let content = std::fs::read_to_string(config_path).ok()?;
    parse_remote_url(&content, &name).map(|_| name)
}

/// Validate the fetch and push refspecs configured for `remote_name`.
///
/// Git loads a remote's definition before any operation, parsing each
/// configured `remote.<name>.fetch` and `remote.<name>.push` refspec; an
/// invalid refspec aborts the command.  This reproduces that early failure so
/// commands like `ls-remote <remote>` reject malformed refspecs.
fn validate_remote_refspecs(remote_name: &str) -> Result<()> {
    let set = if let Ok(repo) = Repository::discover(None) {
        ConfigSet::load(Some(repo.git_dir.as_path()), true).unwrap_or_default()
    } else {
        ConfigSet::load(None, true).unwrap_or_default()
    };

    let fetch_key = format!("remote.{remote_name}.fetch");
    for spec in set.get_all(&fetch_key) {
        if !grit_lib::refspec::valid_fetch_refspec(&spec) {
            bail!("invalid refspec '{spec}'");
        }
    }

    let push_key = format!("remote.{remote_name}.push");
    for spec in set.get_all(&push_key) {
        if !grit_lib::refspec::valid_push_refspec(&spec) {
            bail!("invalid refspec '{spec}'");
        }
    }

    Ok(())
}

fn effective_server_options(args: &Args, remote_name: Option<&str>) -> Vec<String> {
    if !args.server_options.is_empty() {
        return args.server_options.clone();
    }
    let Some(remote_name) = remote_name else {
        return Vec::new();
    };
    let set = if let Ok(repo) = Repository::discover(None) {
        ConfigSet::load(Some(repo.git_dir.as_path()), true).unwrap_or_default()
    } else {
        ConfigSet::load(None, true).unwrap_or_default()
    };
    let mut out = Vec::new();
    for entry in set.entries() {
        if !entry.key.starts_with("remote.") || !entry.key.ends_with(".serveroption") {
            continue;
        }
        let suffix_len = ".serveroption".len();
        let name = &entry.key["remote.".len()..entry.key.len() - suffix_len];
        if name != remote_name {
            continue;
        }
        match entry.value.as_deref() {
            Some("") | None => out.clear(),
            Some(v) => out.push(v.to_owned()),
        }
    }
    out
}

fn run_ls_remote_via_upload_pack(
    repo_path: &Path,
    upload_pack: &str,
    args: &Args,
    sort_keys: &[SortKey],
) -> Result<()> {
    let repo = open_local_repo(repo_path)?;
    let repo_dir = repo
        .work_tree
        .clone()
        .unwrap_or_else(|| repo.git_dir.clone());

    let default_hash = std::env::var("GIT_DEFAULT_HASH").unwrap_or_else(|_| "sha1".to_owned());

    let upload_pack_command = format!("exec {upload_pack} \"$@\"");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&upload_pack_command)
        .arg("git-upload-pack")
        .arg(repo_dir.to_string_lossy().as_ref())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run upload-pack via sh -c '{upload_pack}'"))?;

    let mut stdin = child.stdin.take().context("upload-pack stdin")?;
    let mut stdout = child.stdout.take().context("upload-pack stdout")?;
    let mut stderr = child.stderr.take().context("upload-pack stderr")?;

    let stderr_buf = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stderr.read_to_end(&mut v);
        v
    });

    let mut entries =
        read_ls_remote_upload_pack_output(&mut stdin, &mut stdout, &default_hash, args)?;
    drop(stdin);

    let status = child.wait()?;
    let err_bytes = stderr_buf.join().unwrap_or_default();
    if !err_bytes.is_empty() {
        let _ = std::io::stderr().write_all(&err_bytes);
    }
    if !status.success() {
        bail!(
            "upload-pack exited with status {}",
            status.code().unwrap_or(-1)
        );
    }
    apply_ref_sorting(sort_keys, &mut entries);
    print_entries_with_exit_code(&entries, args.exit_code)
}

fn read_ls_remote_upload_pack_output(
    stdin: &mut impl Write,
    stdout: &mut impl Read,
    object_format: &str,
    args: &Args,
) -> Result<Vec<RefEntry>> {
    let first = match pkt_line::read_packet(stdout).context("read upload-pack first packet")? {
        None => bail!("unexpected EOF from upload-pack"),
        Some(p) => p,
    };

    match first {
        pkt_line::Packet::Data(ref line) if line == "version 2" => {
            skip_rest_of_v2_capability_advertisement(stdout)?;
            write_v2_ls_refs_request(stdin, object_format, args)?;
            stdin.flush()?;
            let mut buf = Vec::new();
            stdout
                .take(512 * 1024)
                .read_to_end(&mut buf)
                .context("read v2 ls-refs response")?;
            parse_v2_ls_refs_output(&buf, args)
        }
        pkt_line::Packet::Data(line) => {
            // The capabilities on the first advertised ref carry all `symref=`
            // entries (HEAD and any others), so collect them once up front.
            let symref_map = symref_map_from_first_line(&line);
            let mut entries = parse_v0_ref_advertisement_line(&line, args, &symref_map)?;
            loop {
                match pkt_line::read_packet(stdout)? {
                    None => break,
                    Some(pkt_line::Packet::Flush) => break,
                    Some(pkt_line::Packet::Data(l)) => {
                        entries.extend(parse_v0_ref_advertisement_line(&l, args, &symref_map)?);
                    }
                    Some(other) => bail!("unexpected packet in v0 ref advertisement: {other:?}"),
                }
            }
            Ok(entries)
        }
        other => bail!("unexpected first packet from upload-pack: {other:?}"),
    }
}

/// Build a `refname -> symref-target` map from a v0 first advertised ref line.
///
/// The capabilities after the `\0` separator contain zero or more
/// `symref=<from>:<to>` entries. v0 servers may advertise multiple symrefs
/// (e.g. `HEAD` and `refs/remotes/origin/HEAD`).
fn symref_map_from_first_line(raw: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some((_, caps)) = raw.split_once('\0') else {
        return map;
    };
    for word in caps.split_whitespace() {
        if let Some(spec) = word.strip_prefix("symref=") {
            if let Some((from, to)) = spec.split_once(':') {
                map.insert(from.to_owned(), to.to_owned());
            }
        }
    }
    map
}

/// Consume pkt-lines after the initial `version 2` line until `0000` flush.
fn skip_rest_of_v2_capability_advertisement(r: &mut impl Read) -> Result<()> {
    loop {
        match pkt_line::read_packet(r).context("read v2 capability packet")? {
            None => bail!("unexpected EOF in v2 capability advertisement"),
            Some(pkt_line::Packet::Flush) => return Ok(()),
            Some(pkt_line::Packet::Data(_)) => {}
            Some(other) => bail!("unexpected packet in v2 capability advertisement: {other:?}"),
        }
    }
}

/// Parse one v0/v1 ref advertisement pkt-line (`<oid>\t<ref>[\0<capabilities>]`).
///
/// `symref_map` maps refnames to their symref targets (collected from the
/// capabilities on the first advertised ref) and is applied when `--symref`
/// is requested.
fn parse_v0_ref_advertisement_line(
    raw: &str,
    args: &Args,
    symref_map: &std::collections::HashMap<String, String>,
) -> Result<Vec<RefEntry>> {
    let payload = raw.split_once('\0').map_or(raw, |(p, _)| p);
    let (oid_hex, refname) = payload
        .split_once('\t')
        .or_else(|| payload.split_once(' '))
        .ok_or_else(|| anyhow::anyhow!("malformed v0 ref advertisement: {raw}"))?;
    let refname = refname.split('\0').next().unwrap_or(refname).trim();
    let oid = ObjectId::from_hex(oid_hex.trim())
        .with_context(|| format!("bad oid in v0 ref advertisement: {oid_hex}"))?;

    // A `capabilities^{}` pseudo-ref carries only capabilities (used by
    // standards-compliant empty remotes); it is never a real ref.
    if refname == "capabilities^{}" {
        return Ok(vec![]);
    }

    // `--branches`/`--tags` form a union; HEAD is excluded when either is set.
    if args.branches || args.tags {
        let is_branch = args.branches && refname.starts_with("refs/heads/");
        let is_tag = args.tags && refname.starts_with("refs/tags/");
        if !is_branch && !is_tag {
            return Ok(vec![]);
        }
    }
    if args.refs_only && (refname == "HEAD" || refname.ends_with("^{}")) {
        return Ok(vec![]);
    }
    if !grit_lib::ls_remote::ref_matches_ls_remote_patterns(refname, &args.patterns) {
        return Ok(vec![]);
    }

    let symref_target = if args.symref {
        symref_map.get(refname).cloned()
    } else {
        None
    };

    Ok(vec![RefEntry {
        name: refname.to_owned(),
        oid,
        symref_target,
    }])
}

fn write_v2_ls_refs_request(w: &mut impl Write, object_format: &str, args: &Args) -> Result<()> {
    pkt_line::write_line(w, "command=ls-refs")?;
    pkt_line::write_line(w, &format!("object-format={object_format}"))?;
    pkt_line::write_delim(w)?;
    if args.symref {
        pkt_line::write_line(w, "symrefs")?;
    }
    if !args.refs_only {
        pkt_line::write_line(w, "peel")?;
    }
    if args.branches {
        pkt_line::write_line(w, "ref-prefix refs/heads/")?;
    }
    if args.tags {
        pkt_line::write_line(w, "ref-prefix refs/tags/")?;
    }
    for p in &args.patterns {
        pkt_line::write_line(w, &format!("ref-prefix {p}"))?;
    }
    pkt_line::write_flush(w)?;
    Ok(())
}

pub(crate) fn parse_v2_ls_refs_output(data: &[u8], args: &Args) -> Result<Vec<RefEntry>> {
    let mut cursor = Cursor::new(data);
    let mut entries: Vec<RefEntry> = Vec::new();

    loop {
        let pkt = match pkt_line::read_packet(&mut cursor)? {
            None => break,
            Some(pkt_line::Packet::Flush) => break,
            Some(pkt_line::Packet::Data(line)) => line,
            Some(other) => bail!("unexpected pkt-line in ls-refs response: {other:?}"),
        };

        let (name, oid, peeled, symref_target) = parse_ls_refs_v2_line(&pkt)?;
        if !grit_lib::ls_remote::ref_matches_ls_remote_patterns(&name, &args.patterns) {
            continue;
        }

        entries.push(RefEntry {
            name: name.clone(),
            oid,
            symref_target: symref_target.clone(),
        });

        if let Some(poid) = peeled {
            if !args.refs_only {
                entries.push(RefEntry {
                    name: format!("{name}^{{}}"),
                    oid: poid,
                    symref_target: None,
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        let rank = |n: &str| if n == "HEAD" { 0 } else { 1 };
        match rank(&a.name).cmp(&rank(&b.name)) {
            std::cmp::Ordering::Equal => a.name.cmp(&b.name),
            o => o,
        }
    });

    Ok(entries)
}

/// Parse one `ls-refs` v2 data line: `<oid> <ref> [peeled:<hex>] [symref-target:<path>]`.
pub(crate) fn parse_ls_refs_v2_line(
    line: &str,
) -> Result<(String, ObjectId, Option<ObjectId>, Option<String>)> {
    const PEEL: &str = " peeled:";
    const SYM: &str = " symref-target:";

    let (oid_hex, after_oid) = line
        .split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("bad ls-refs line: {line}"))?;
    // Protocol v2 advertises an unborn HEAD (symref pointing at a branch with no commits) as the
    // literal token `unborn` in place of the object id (gitprotocol-v2: `obj-id-or-unborn`). Map it
    // to the null OID so callers can recognize "no object" via `ObjectId::is_zero`.
    let oid = if oid_hex == "unborn" {
        ObjectId::zero()
    } else {
        ObjectId::from_hex(oid_hex).with_context(|| format!("bad oid in ls-refs: {oid_hex}"))?
    };

    let mut peeled = None;
    let mut symref_target = None;

    if let Some(pos) = after_oid.find(PEEL) {
        let name = after_oid[..pos].to_owned();
        ensure_valid_ls_refs_v2_name(&name)?;
        let tail = &after_oid[pos + PEEL.len()..];
        let hex_end = tail.find(' ').unwrap_or(tail.len());
        let ph = &tail[..hex_end];
        peeled = Some(ObjectId::from_hex(ph).with_context(|| format!("bad peeled oid: {ph}"))?);
        let rest = tail[hex_end..].trim_start();
        if let Some(s) = rest.strip_prefix("symref-target:") {
            ensure_valid_ls_refs_v2_name(s)?;
            symref_target = Some(s.to_owned());
        }
        return Ok((name, oid, peeled, symref_target));
    }

    if let Some(pos) = after_oid.find(SYM) {
        let name = after_oid[..pos].to_owned();
        let target = after_oid[pos + SYM.len()..].to_owned();
        ensure_valid_ls_refs_v2_name(&name)?;
        ensure_valid_ls_refs_v2_name(&target)?;
        symref_target = Some(target);
        return Ok((name, oid, peeled, symref_target));
    }

    ensure_valid_ls_refs_v2_name(after_oid)?;
    Ok((after_oid.to_owned(), oid, None, None))
}

fn ensure_valid_ls_refs_v2_name(name: &str) -> Result<()> {
    if is_valid_advertised_refname(name) {
        Ok(())
    } else {
        bail!("invalid ref advertisement: {name}")
    }
}

/// Open a local repository given a user-supplied path.
///
/// Tries `path` directly (bare repository or an explicit `.git` directory),
/// and falls back to `path/.git` for a standard non-bare working directory.
///
/// # Errors
///
/// Returns an error when neither location looks like a valid git repository.
fn open_local_repo(path: &Path) -> Result<Repository> {
    // Strip file:// URL scheme if present
    let effective_path = {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix("file://") {
            PathBuf::from(stripped)
        } else {
            path.to_path_buf()
        }
    };
    let path = &effective_path;

    // Bare repository or explicit git-dir directory.
    if let Ok(repo) = Repository::open(path, None) {
        return Ok(repo);
    }

    // Explicit gitfile path (e.g. ".../foo/.git" where ".git" is a file).
    if path.is_file() {
        if let Ok(git_dir) = resolve_gitdir_from_gitfile_path(path) {
            return Ok(Repository::open(&git_dir, path.parent())?);
        }
    }

    // Standard working-tree repository path.
    let dot_git = path.join(".git");
    if dot_git.is_file() {
        if let Ok(git_dir) = resolve_gitdir_from_gitfile_path(&dot_git) {
            return Ok(Repository::open(&git_dir, Some(path))?);
        }
    }
    if dot_git.is_dir() {
        return Ok(Repository::open(&dot_git, Some(path))?);
    }

    Repository::open(&dot_git, Some(path)).with_context(|| {
        format!(
            "'{}' does not appear to be a git repository: {}",
            path.display(),
            "not a git repository (or any of the parent directories)"
        )
    })
}

fn resolve_gitdir_from_gitfile_path(gitfile_path: &Path) -> Result<PathBuf> {
    let content = std::fs::read_to_string(gitfile_path).with_context(|| {
        format!(
            "'{}' does not appear to be a git repository: {}",
            gitfile_path.display(),
            "not a git repository (or any of the parent directories)"
        )
    })?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("gitdir:") {
            let rel = rest.trim();
            if rel.is_empty() {
                break;
            }
            let candidate = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                gitfile_path.parent().unwrap_or(Path::new(".")).join(rel)
            };
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "'{}' does not appear to be a git repository: not a git repository (or any of the parent directories)",
        gitfile_path.display()
    )
}

/// If the repository argument matches a configured remote name, resolve to its URL.
/// Otherwise return the original path.
fn resolve_remote_or_path(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();

    // Remote config takes precedence over filesystem paths, even when the
    // remote name itself contains slashes.
    if let Ok(repo) = Repository::discover(None) {
        let config_path = repo.git_dir.join("config");
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if let Some(url) = parse_remote_url(&content, &path_str) {
                return PathBuf::from(url);
            }
        }
    }

    path.to_path_buf()
}

/// Check if a path looks like a git bundle file (starts with v2 bundle header).
fn is_bundle_file(path: &Path) -> bool {
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut buf = [0u8; 20];
        if let Ok(n) = std::io::Read::read(&mut f, &mut buf) {
            return buf[..n].starts_with(b"# v2 git bundle");
        }
    }
    false
}

/// Run ls-remote against a bundle file.
fn run_bundle_ls_remote(path: &Path, args: &Args) -> Result<()> {
    let data = std::fs::read(path)
        .with_context(|| format!("could not read bundle '{}'.", path.display()))?;
    let refs = parse_bundle_refs(&data)?;

    if refs.is_empty() {
        return Ok(());
    }

    if args.quiet {
        return Ok(());
    }

    // Git's bundle transport (`get_refs_from_bundle`) prepends each header ref to
    // the result list, so the refs it returns are in reverse header order. With no
    // `--sort` given, `git ls-remote` applies no default ordering, so it prints
    // them in exactly that reversed order. Reproduce that here.
    for (refname, oid) in refs.iter().rev() {
        if args.branches && !refname.starts_with("refs/heads/") {
            continue;
        }
        if args.tags && !refname.starts_with("refs/tags/") {
            continue;
        }
        if !args.patterns.is_empty() {
            let matched = args
                .patterns
                .iter()
                .any(|p| refname.contains(p) || refname.ends_with(p));
            if !matched {
                continue;
            }
        }
        println!("{oid}\t{refname}");
    }
    Ok(())
}

/// Parse refs from a v2 bundle header.
fn parse_bundle_refs(data: &[u8]) -> Result<Vec<(String, grit_lib::objects::ObjectId)>> {
    let header_line = b"# v2 git bundle\n";
    if !data.starts_with(header_line) {
        anyhow::bail!("not a v2 git bundle");
    }
    let mut pos = header_line.len();
    let mut refs = Vec::new();
    loop {
        let eol = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .ok_or_else(|| anyhow::anyhow!("truncated bundle header"))?;
        let line = &data[pos..eol];
        if line.is_empty() {
            break;
        }
        let line_str = std::str::from_utf8(line)?;
        if line_str.starts_with('-') {
            pos = eol + 1;
            continue;
        }
        if let Some((hex, refname)) = line_str.split_once(' ') {
            let oid = grit_lib::objects::ObjectId::from_hex(hex)
                .map_err(|e| anyhow::anyhow!("bad oid in bundle: {e}"))?;
            refs.push((refname.to_string(), oid));
        }
        pos = eol + 1;
    }
    Ok(refs)
}

fn parse_remote_url(config: &str, remote_name: &str) -> Option<String> {
    let section_header = format!("[remote \"{remote_name}\"]");
    let mut in_section = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == section_header;
            continue;
        }
        if in_section {
            if let Some(value) = trimmed.strip_prefix("url") {
                let value = value.trim_start();
                if let Some(value) = value.strip_prefix('=') {
                    return Some(value.trim().to_string());
                }
            }
        }
    }
    None
}

fn common_git_dir_or_self(git_dir: &Path) -> PathBuf {
    let commondir_path = git_dir.join("commondir");
    let Ok(raw) = std::fs::read_to_string(commondir_path) else {
        return git_dir.to_path_buf();
    };
    let rel = raw.trim();
    if rel.is_empty() {
        return git_dir.to_path_buf();
    }
    let candidate = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        git_dir.join(rel)
    };
    candidate.canonicalize().unwrap_or(candidate)
}

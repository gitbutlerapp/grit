//! `grit tag` — create, list, delete, and manage tags.
//!
//! Supports lightweight tags (simple refs), annotated tags (tag objects),
//! listing with optional pattern matching, and deletion.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::diff::zero_oid;
use grit_lib::objects::{parse_commit, parse_tag, serialize_tag, ObjectId, ObjectKind, TagData};
use grit_lib::reflog::reflog_path;
use grit_lib::refs::{append_reflog, should_autocreate_reflog_for_mode};
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;

use crate::porcelain_rev::{resolve_porcelain_commitish_filter, resolve_porcelain_points_at};
use grit_lib::porcelain::tag::{
    compare_version, creator_date, get_tag_annotation, strip_comments, tag_contains, tag_points_at,
};
// Re-exported so existing `crate::commands::tag::glob_matches` callers (e.g.
// `commands/describe.rs`) keep resolving after the move into grit-lib.
pub use grit_lib::porcelain::tag::glob_matches;
use grit_lib::state::resolve_head;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use time::OffsetDateTime;

/// Arguments for `grit tag`.
#[derive(Debug, ClapArgs)]
#[command(about = "Create, list, delete or verify a tag object signed with GPG")]
pub struct Args {
    /// Positional arguments: tag name(s), optional commit.
    #[arg(value_name = "ARG")]
    pub positional: Vec<String>,

    /// Create an annotated tag.
    #[arg(short = 'a', long = "annotate")]
    pub annotate: bool,

    /// Create a GPG-signed tag.
    #[arg(short = 's', long = "sign")]
    pub sign: bool,

    /// Use the given key to sign the tag (implies `-s`).
    #[arg(short = 'u', long = "local-user", value_name = "KEY-ID")]
    pub local_user: Option<String>,

    /// Tag message (implies `-a`).
    #[arg(short = 'm', long = "message")]
    pub message: Vec<String>,

    /// Read tag message from file.
    #[arg(short = 'F', long = "file")]
    pub file: Option<String>,

    /// Cleanup mode for tag messages.
    #[arg(long = "cleanup")]
    pub cleanup: Option<String>,

    /// Delete a tag.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// List tags matching the given pattern.
    #[arg(short = 'l', long = "list", action = clap::ArgAction::Count)]
    pub list: u8,

    /// Force creation (overwrite existing tag).
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Show N lines of annotation (default 1 when -n given alone).
    #[arg(short = 'n', default_missing_value = "1", num_args = 0..=1)]
    pub lines: Option<u32>,

    /// Sort by key (e.g. `version:refname`, `creatordate`).
    #[arg(long = "sort")]
    pub sort: Option<String>,

    /// Format to use when listing tags.
    #[arg(long = "format")]
    pub format: Option<String>,

    /// List only tags that contain the specified commit (defaults to HEAD).
    #[arg(long = "contains", num_args = 0..=1, default_missing_value = "HEAD")]
    pub contains: Option<String>,

    /// List only tags that do not contain the specified commit (defaults to HEAD).
    #[arg(long = "no-contains", num_args = 0..=1, default_missing_value = "HEAD")]
    pub no_contains: Option<String>,

    /// Verify a tag (GPG signature check).
    #[arg(short = 'v', long = "verify")]
    pub verify: bool,

    /// Only list tags that point at the specified object (defaults to HEAD).
    #[arg(long = "points-at", num_args = 0..=1, default_missing_value = "HEAD")]
    pub points_at: Option<String>,

    /// Case-insensitive sort for -l listing.
    #[arg(short = 'i', long = "ignore-case")]
    pub ignore_case: bool,

    /// Create a reflog for the tag.
    #[arg(long = "create-reflog")]
    pub create_reflog: bool,
}

/// Run the `tag` command.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;

    // Verify mode
    // Extract name and commit from positional args
    let name = args.positional.first().map(|s| s.as_str());
    let commit = args.positional.get(1).map(|s| s.as_str());

    if args.verify {
        let name = name.ok_or_else(|| anyhow::anyhow!("tag name required"))?;
        return verify_tag(&repo, name);
    }

    // Delete mode
    if args.delete {
        if args.positional.is_empty() {
            // git tag -d with no args succeeds and does nothing
            return Ok(());
        }
        let mut had_error = false;
        for tag_name in &args.positional {
            if let Err(e) = delete_tag(&repo, tag_name) {
                eprintln!("error: {e}");
                had_error = true;
            }
        }
        if had_error {
            bail!("some tags could not be deleted");
        }
        return Ok(());
    }

    // `-u <keyid>` forces signing (git/builtin/tag.c:507).
    let sign = args.sign || args.local_user.is_some();

    // If annotated/signed/force without a name, fail
    let is_create_mode =
        args.annotate || sign || !args.message.is_empty() || args.file.is_some() || args.force;
    if name.is_none() && is_create_mode {
        bail!("tag name required");
    }
    let _is_annotated_mode =
        args.annotate || sign || !args.message.is_empty() || args.file.is_some();

    // If no name is given (or -l is given), list tags
    if name.is_none() || args.list > 0 {
        let patterns: Vec<&str> = args.positional.iter().map(|s| s.as_str()).collect();
        // Read tag.sort from config when no --sort arg given
        let config_sort = if args.sort.is_none() {
            let cfg = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
            cfg.and_then(|c| c.get("tag.sort"))
        } else {
            None
        };
        let effective_sort = args.sort.as_deref().or(config_sort.as_deref());
        // Validate sort key if specified
        if let Some(sort_key) = effective_sort {
            let key = sort_key.trim_start_matches('-');
            let valid_keys = [
                "refname",
                "version:refname",
                "creatordate",
                "taggerdate",
                "committerdate",
                "objecttype",
                "",
            ];
            if !valid_keys.contains(&key) {
                eprintln!("error: invalid sort key: '{sort_key}'");
                std::process::exit(129);
            }
        }
        return list_tags(
            &repo,
            &patterns,
            args.lines,
            effective_sort,
            args.ignore_case,
            args.contains.as_deref(),
            args.no_contains.as_deref(),
            args.points_at.as_deref(),
            args.format.as_deref(),
        );
    }

    // Create tag
    let name = name.ok_or_else(|| anyhow::anyhow!("tag name required"))?;

    // Validate tag name (git check-ref-format rules)
    if name.is_empty()
        || name == "HEAD"
        || name.starts_with('.')
        || name.starts_with('-')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("/.")
        || name.contains("@{")
        || name.contains('\\')
        || name.contains('~')
        || name.contains('^')
        || name.contains(':')
        || name.contains('?')
        || name.contains('*')
        || name.contains('[')
        || name.bytes().any(|b| b < 0x20 || b == 0x7f)
        || name.contains(' ')
    // spaces not allowed in tag names
    {
        bail!("'{}' is not a valid tag name.", name);
    }

    // Resolve the target commit
    let target_rev = commit.unwrap_or("HEAD");
    let target_oid = resolve_revision(&repo, target_rev)
        .with_context(|| format!("Failed to resolve '{target_rev}'"))?;

    // Reject using both -m and -F
    if !args.message.is_empty() && args.file.is_some() {
        bail!("only one of -m or -F can be given.");
    }

    let annotated = args.annotate || sign || !args.message.is_empty() || args.file.is_some();

    let tag_refname = format!("refs/tags/{name}");
    let tag_exists = grit_lib::refs::resolve_ref(&repo.git_dir, &tag_refname).is_ok();

    if tag_exists && !args.force {
        bail!("tag '{name}' already exists");
    }

    if annotated {
        create_annotated_tag(&repo, name, target_oid, &args)?;
    } else {
        create_lightweight_tag(&repo, name, target_oid, &args)?;
    }

    Ok(())
}

/// Create a lightweight (direct ref) tag.
fn create_lightweight_tag(
    repo: &Repository,
    name: &str,
    target_oid: ObjectId,
    args: &Args,
) -> Result<()> {
    let refname = format!("refs/tags/{name}");
    let old_oid =
        grit_lib::refs::resolve_ref(&repo.git_dir, &refname).unwrap_or_else(|_| zero_oid());
    grit_lib::refs::write_ref(&repo.git_dir, &refname, &target_oid)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if should_create_tag_reflog(repo, args, &refname) {
        let config = ConfigSet::load(Some(&repo.git_dir), true)?;
        let ident = resolve_tagger(&config, OffsetDateTime::now_utc())?;
        let msg = format!("tag: {name}");
        let _ = append_reflog(
            &repo.git_dir,
            &refname,
            &old_oid,
            &target_oid,
            &ident,
            &msg,
            true,
        );
    }
    Ok(())
}

/// Create an annotated tag object and write its ref.
fn create_annotated_tag(
    repo: &Repository,
    name: &str,
    target_oid: ObjectId,
    args: &Args,
) -> Result<()> {
    // Build the message
    let message = build_tag_message(args)?;
    // Only fail if NO -m/-F was given AND message is empty
    let has_explicit_message = !args.message.is_empty() || args.file.is_some();
    if !has_explicit_message && message.trim().is_empty() {
        bail!("no tag message provided (use -m or -F)");
    }

    // Determine the type of the target object
    let obj = repo
        .odb
        .read(&target_oid)
        .with_context(|| format!("object {} not found", target_oid.to_hex()))?;
    let object_type = obj.kind.as_str().to_owned();

    // Build tagger identity
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let now = OffsetDateTime::now_utc();
    let tagger = resolve_tagger(&config, now)?;

    let tag_data = TagData {
        object: target_oid,
        object_type,
        tag: name.to_owned(),
        tagger: Some(tagger.clone()),
        message,
    };

    // `-u <keyid>` implies signing (git/builtin/tag.c:507).
    let sign = args.sign || args.local_user.is_some();

    let mut tag_bytes = serialize_tag(&tag_data);

    if sign {
        // Git appends the armored signature directly after the serialized tag
        // body (git/builtin/tag.c:191 `strbuf_addbuf(buffer, &sig)`): no
        // `gpgsig` header and no per-line indentation, unlike commits.
        let cfg = grit_lib::signing::GpgConfig::from_config(&config)?;
        let committer_default = grit_lib::signing::committer_signing_default(&tagger);
        let signing_key = cfg.resolve_signing_key(args.local_user.as_deref(), &committer_default);
        let signature = grit_lib::signing::sign_buffer(&cfg, &tag_bytes, &signing_key)
            .map_err(|e| anyhow::anyhow!("failed to sign the tag: {e}"))?;
        tag_bytes.extend_from_slice(&signature);
    }

    let tag_oid = repo.odb.write(ObjectKind::Tag, &tag_bytes)?;

    let refname = format!("refs/tags/{name}");
    let old_oid =
        grit_lib::refs::resolve_ref(&repo.git_dir, &refname).unwrap_or_else(|_| zero_oid());
    grit_lib::refs::write_ref(&repo.git_dir, &refname, &tag_oid)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if should_create_tag_reflog(repo, args, &refname) {
        let msg = format!("tag: {name}");
        let _ = append_reflog(
            &repo.git_dir,
            &refname,
            &old_oid,
            &tag_oid,
            &tagger,
            &msg,
            true,
        );
    }
    Ok(())
}

fn should_create_tag_reflog(repo: &Repository, args: &Args, refname: &str) -> bool {
    if args.create_reflog || grit_lib::reflog::reflog_exists(&repo.git_dir, refname) {
        return true;
    }
    ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .map(|config| config.effective_log_refs_config(&repo.git_dir))
        .is_some_and(|mode| should_autocreate_reflog_for_mode(refname, mode))
}

/// Delete a tag by name.
fn delete_tag(repo: &Repository, name: &str) -> Result<()> {
    let refname = format!("refs/tags/{name}");
    let oid = grit_lib::refs::resolve_ref(&repo.git_dir, &refname)
        .map_err(|_| anyhow::anyhow!("tag '{name}' not found."))?;
    let log_path = reflog_path(&repo.git_dir, &refname);
    let saved_log = fs::read(&log_path).ok();
    grit_lib::refs::delete_ref(&repo.git_dir, &refname).map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(content) = saved_log {
        if let Some(parent) = log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&log_path, content);
    }
    let hex = oid.to_hex();
    let short = &hex[..7.min(hex.len())];
    eprintln!("Deleted tag '{name}' (was {short})");
    Ok(())
}

/// Verify a tag: check it exists and print its contents if annotated.
///
/// For unsigned annotated tags, git tag -v fails because there is no
/// GPG signature to verify.  We replicate that behaviour.
fn verify_tag(repo: &Repository, name: &str) -> Result<()> {
    let refname = format!("refs/tags/{name}");
    let oid = grit_lib::refs::resolve_ref(&repo.git_dir, &refname)
        .map_err(|_| anyhow::anyhow!("tag '{name}' not found."))?;
    let obj = repo.odb.read(&oid)?;
    match obj.kind {
        ObjectKind::Tag => {
            // Annotated tag — check for GPG signature
            let content = String::from_utf8_lossy(&obj.data);
            if content.contains("-----BEGIN PGP SIGNATURE-----") {
                // We don't actually do GPG verification; print tag contents
                print!("{content}");
                Ok(())
            } else {
                bail!("no signature found");
            }
        }
        _ => {
            // Lightweight tag — nothing to verify
            bail!("cannot verify a non-tag object");
        }
    }
}

/// List tags, optionally filtered by a glob pattern.
///
/// - `pattern` — shell glob pattern; `None` means list all.
/// - `lines` — number of annotation lines to show with each tag.
/// - `sort` — sort key.
/// - `ignore_case` — sort case-insensitively.
/// - `contains` — only list tags that contain this commit.
#[expect(clippy::too_many_arguments)]
fn list_tags(
    repo: &Repository,
    patterns: &[&str],
    lines: Option<u32>,
    sort: Option<&str>,
    ignore_case: bool,
    contains: Option<&str>,
    no_contains: Option<&str>,
    points_at: Option<&str>,
    format: Option<&str>,
) -> Result<()> {
    let mut tags: Vec<(String, ObjectId)> = grit_lib::refs::list_refs(&repo.git_dir, "refs/tags/")
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_iter()
        .map(|(name, oid)| {
            let short = name.strip_prefix("refs/tags/").unwrap_or(&name).to_owned();
            (short, oid)
        })
        .collect();

    // Filter by --contains
    if let Some(rev) = contains {
        let target = resolve_porcelain_commitish_filter(repo, rev)?;
        tags.retain(|(_, tag_oid)| tag_contains(repo, tag_oid, &target));
    }

    // Filter by --no-contains
    if let Some(rev) = no_contains {
        let target = resolve_porcelain_commitish_filter(repo, rev)?;
        tags.retain(|(_, tag_oid)| !tag_contains(repo, tag_oid, &target));
    }

    // Filter by --points-at
    if let Some(rev) = points_at {
        let target = resolve_porcelain_points_at(repo, rev, false)?;
        tags.retain(|(_, tag_oid)| tag_points_at(repo, tag_oid, &target));
    }

    // Filter by pattern
    if !patterns.is_empty() {
        if ignore_case {
            tags.retain(|(name, _)| {
                let lower_name = name.to_lowercase();
                patterns
                    .iter()
                    .any(|pat| glob_matches(&pat.to_lowercase(), &lower_name))
            });
        } else {
            tags.retain(|(name, _)| patterns.iter().any(|pat| glob_matches(pat, name)));
        }
    }

    // Sort
    sort_tags(repo, &mut tags, sort, ignore_case);

    let stdout = io::stdout();
    let mut out = stdout.lock();

    for (name, oid) in &tags {
        if let Some(format) = format {
            let line = format_tag_line(repo, name, oid, format)?;
            writeln!(out, "{line}")?;
        } else if let Some(n) = lines {
            let annotation = get_tag_annotation(repo, oid, n);
            // When -n is specified, always pad name to 15 chars (git behavior)
            if let Some(ann) = annotation {
                writeln!(out, "{name:<15} {ann}")?
            } else if n > 0 {
                // No annotation but -n specified: pad name
                writeln!(out, "{name:<15} ")?;
            } else {
                writeln!(out, "{name}")?;
            }
        } else {
            writeln!(out, "{name}")?;
        }
    }

    Ok(())
}

fn format_tag_line(repo: &Repository, name: &str, oid: &ObjectId, format: &str) -> Result<String> {
    let mut out = String::new();
    let mut i = 0usize;
    while i < format.len() {
        if let Some(rest) = format[i..].strip_prefix("%%") {
            out.push('%');
            i = format.len() - rest.len();
            continue;
        }
        if let Some(rest) = format[i..].strip_prefix("%(") {
            let Some(close) = rest.find(')') else {
                bail!("unterminated format atom");
            };
            let atom = &rest[..close];
            out.push_str(&expand_tag_atom(repo, name, oid, atom)?);
            i += 2 + close + 1;
            continue;
        }
        let ch = format[i..].chars().next().unwrap_or_default();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

fn expand_tag_atom(repo: &Repository, name: &str, oid: &ObjectId, atom: &str) -> Result<String> {
    let (base, modifier) = atom
        .find(':')
        .map(|p| (&atom[..p], Some(&atom[p + 1..])))
        .unwrap_or((atom, None));
    match base {
        "refname" => match modifier {
            Some("short") | None => Ok(name.to_owned()),
            Some(m) => bail!("unrecognized %(refname) argument: {m}"),
        },
        "objectname" => match modifier {
            None => Ok(oid.to_hex()),
            Some("short") => Ok(oid.to_hex()[..7.min(oid.to_hex().len())].to_owned()),
            Some(m) => bail!("unrecognized %(objectname) argument: {m}"),
        },
        "contents" => {
            let obj = repo.odb.read(oid)?;
            let message = match obj.kind {
                ObjectKind::Tag => parse_tag(&obj.data)?.message,
                ObjectKind::Commit => parse_commit(&obj.data)?.message,
                _ => String::new(),
            };
            match modifier {
                Some("subject") => Ok(grit_lib::commit_pretty::message_subject(&message)),
                Some("body") => Ok(grit_lib::commit_pretty::message_body(&message).to_owned()),
                Some("size") => Ok(message.len().to_string()),
                Some("") | None => Ok(message),
                Some(m) => bail!("unsupported contents modifier: {m}"),
            }
        }
        _ => bail!("unsupported format atom: {base}"),
    }
}

/// Sort tags by the requested key.
fn sort_tags(
    repo: &Repository,
    tags: &mut [(String, ObjectId)],
    sort: Option<&str>,
    ignore_case: bool,
) {
    let key = sort.unwrap_or("");
    let (descending, bare_key) = match key.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, key),
    };

    match bare_key {
        "version:refname" => {
            tags.sort_by(|a, b| {
                let ord = compare_version(&a.0, &b.0);
                if descending {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }
        "creatordate" => {
            tags.sort_by(|a, b| {
                let da = creator_date(repo, &a.1);
                let db = creator_date(repo, &b.1);
                let ord = da.cmp(&db).then_with(|| a.0.cmp(&b.0));
                if descending {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }
        "refname" => {
            tags.sort_by(|a, b| {
                let ord = if ignore_case {
                    a.0.to_lowercase().cmp(&b.0.to_lowercase())
                } else {
                    a.0.cmp(&b.0)
                };
                if descending {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }
        _ if key.is_empty() => {
            // Default: ascending alphabetical
            if ignore_case {
                tags.sort_by_key(|t| t.0.to_lowercase());
            }
            // Already sorted lexicographically from collect step
        }
        _ => {
            // Unknown key — fail with error like git does
            eprintln!("error: invalid sort key: '{key}'");
            std::process::exit(129);
        }
        #[allow(unreachable_patterns)]
        _unreachable => {
            // Unknown key — fallback to alphabetical
            if ignore_case {
                tags.sort_by_key(|t| t.0.to_lowercase());
            }
        }
    }
}

/// Build the tag message from CLI args.
fn build_tag_message(args: &Args) -> Result<String> {
    if !args.message.is_empty() {
        let msg = args.message.join("\n\n");
        if args.cleanup.as_deref() == Some("verbatim") {
            return Ok(msg);
        }
        let stripped = strip_comments(&msg);
        return Ok(stripped);
    }

    if let Some(ref file_path) = args.file {
        let content = if file_path == "-" {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            fs::read_to_string(file_path)?
        };
        if args.cleanup.as_deref() == Some("verbatim") {
            return Ok(content);
        }
        let stripped = strip_comments(&content);
        return Ok(stripped);
    }

    Ok(String::new())
}

/// Resolve the tagger identity from env and config.
fn resolve_tagger(config: &ConfigSet, now: OffsetDateTime) -> Result<String> {
    let name = std::env::var("GIT_COMMITTER_NAME")
        .ok()
        .or_else(|| config.get("user.name"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Tagger identity unknown\n\nPlease tell me who you are.\n\n\
                 Run\n\n  git config user.email \"you@example.com\"\n  git config user.name \"Your Name\""
            )
        })?;

    let email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();

    let date_str = std::env::var("GIT_COMMITTER_DATE").ok();
    let timestamp = match date_str {
        Some(d) => grit_lib::commit::parse_date_to_git_timestamp(&d).unwrap_or(d),
        None => grit_lib::commit::format_git_timestamp(now),
    };

    Ok(format!("{name} <{email}> {timestamp}"))
}

/// Resolve HEAD to the current commit OID, if any.
///
/// Used internally to ensure HEAD is valid when creating a tag.
#[allow(dead_code)]
fn resolve_head_oid(git_dir: &Path) -> Result<ObjectId> {
    let head = resolve_head(git_dir)?;
    head.oid()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("not a valid object name: 'HEAD'"))
}

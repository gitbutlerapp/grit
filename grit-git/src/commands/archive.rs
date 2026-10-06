//! `grit archive` — create a tar/zip archive of a tree.
//!
//! Implements behaviors required by `t5000-tar-tree`: commit mtimes, pax global `comment`,
//! `export-ignore` / `export-subst`, ordered `--prefix` / `--add-file`, pathspecs, gzip and
//! `tar.*` config filters, and `--remote` via the upload-archive protocol.

use crate::commands::describe::{describe_object, DescribeOptions};
use crate::explicit_exit::ExplicitExit;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use flate2::write::GzEncoder;
use flate2::Compression;
use grit_lib::config::ConfigSet;
use grit_lib::crlf::{
    convert_to_worktree_eager, get_file_attrs, load_gitattributes, ConversionConfig,
};
use grit_lib::git_date::parse::parse_date_basic;
use grit_lib::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use grit_lib::refs::{list_refs, resolve_ref};
use grit_lib::repo::Repository;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use grit_lib::pkt_line;

const DEFAULT_MAX_TREE_DEPTH: usize = 2048;
const USTAR_MAX: u64 = 0o777_7777_7777;
const TAR_RECORD_SIZE: usize = 512;
const TAR_BLOCK_SIZE: usize = TAR_RECORD_SIZE * 20;

/// Legacy clap-based entry (unused by the main dispatcher; kept for `--git-completion-helper`).
#[derive(Debug, ClapArgs)]
#[command(about = "Create an archive of files from a named tree")]
pub struct Args {}

/// Run from raw argv after `archive` (Git-compatible option ordering).
pub fn run_from_argv(rest: &[String]) -> Result<()> {
    crate::commands::upstream_synopsis_help::try_print_upstream_help_and_exit("archive", rest);
    execute(parse_archive_argv(rest)?)
}

#[derive(Debug, Clone)]
pub(crate) enum ArchiveToken {
    List,
    Remote(String),
    Exec(String),
    Output(String),
    Format(String),
    Prefix(String),
    AddFile(PathBuf),
    Mtime(String),
    Verbose,
    WorktreeAttributes,
    EndOfOptions,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedArchive {
    pub(crate) tokens: Vec<ArchiveToken>,
    pub(crate) tree_ish: Option<String>,
    pub(crate) pathspecs: Vec<String>,
}

pub(crate) fn parse_archive_argv(rest: &[String]) -> Result<ParsedArchive> {
    let mut p = ParsedArchive::default();
    let mut i = 0usize;
    let mut end_opts = false;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "--end-of-options" {
            p.tokens.push(ArchiveToken::EndOfOptions);
            end_opts = true;
            i += 1;
            continue;
        }
        if !end_opts && a == "--" {
            i += 1;
            while i < rest.len() {
                p.pathspecs.push(rest[i].clone());
                i += 1;
            }
            break;
        }
        if !end_opts && a.starts_with('-') {
            match a {
                "--list" => {
                    p.tokens.push(ArchiveToken::List);
                    i += 1;
                }
                "--remote" => {
                    let v = rest
                        .get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("option `--remote` requires a value"))?;
                    p.tokens.push(ArchiveToken::Remote(v.clone()));
                    i += 2;
                }
                s if s.starts_with("--remote=") => {
                    p.tokens.push(ArchiveToken::Remote(
                        s.trim_start_matches("--remote=").to_string(),
                    ));
                    i += 1;
                }
                "--exec" => {
                    let v = rest
                        .get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("option `--exec` requires a value"))?;
                    p.tokens.push(ArchiveToken::Exec(v.clone()));
                    i += 2;
                }
                s if s.starts_with("--exec=") => {
                    p.tokens.push(ArchiveToken::Exec(
                        s.trim_start_matches("--exec=").to_string(),
                    ));
                    i += 1;
                }
                "-o" | "--output" => {
                    let v = rest
                        .get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("option `--output` requires a value"))?;
                    p.tokens.push(ArchiveToken::Output(v.clone()));
                    i += 2;
                }
                s if s.starts_with("--output=") => {
                    p.tokens.push(ArchiveToken::Output(
                        s.trim_start_matches("--output=").to_string(),
                    ));
                    i += 1;
                }
                "--format" => {
                    let v = rest
                        .get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("option `--format` requires a value"))?;
                    p.tokens.push(ArchiveToken::Format(v.clone()));
                    i += 2;
                }
                s if s.starts_with("--format=") => {
                    p.tokens.push(ArchiveToken::Format(
                        s.trim_start_matches("--format=").to_string(),
                    ));
                    i += 1;
                }
                "--prefix" => {
                    let v = rest
                        .get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("option `--prefix` requires a value"))?;
                    p.tokens.push(ArchiveToken::Prefix(v.clone()));
                    i += 2;
                }
                s if s.starts_with("--prefix=") => {
                    p.tokens.push(ArchiveToken::Prefix(
                        s.trim_start_matches("--prefix=").to_string(),
                    ));
                    i += 1;
                }
                "--add-file" => {
                    let v = rest
                        .get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("option `--add-file` requires a value"))?;
                    p.tokens.push(ArchiveToken::AddFile(PathBuf::from(v)));
                    i += 2;
                }
                s if s.starts_with("--add-file=") => {
                    p.tokens.push(ArchiveToken::AddFile(PathBuf::from(
                        s.trim_start_matches("--add-file="),
                    )));
                    i += 1;
                }
                "--mtime" => {
                    let v = rest
                        .get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("option `--mtime` requires a value"))?;
                    p.tokens.push(ArchiveToken::Mtime(v.clone()));
                    i += 2;
                }
                s if s.starts_with("--mtime=") => {
                    p.tokens.push(ArchiveToken::Mtime(
                        s.trim_start_matches("--mtime=").to_string(),
                    ));
                    i += 1;
                }
                "-v" | "--verbose" => {
                    p.tokens.push(ArchiveToken::Verbose);
                    i += 1;
                }
                "-0" | "-1" | "-2" | "-3" | "-4" | "-5" | "-6" | "-7" | "-8" | "-9" => {
                    i += 1;
                }
                "--worktree-attributes" => {
                    p.tokens.push(ArchiveToken::WorktreeAttributes);
                    i += 1;
                }
                _ => bail!("unknown option: {a}"),
            }
            continue;
        }
        if p.tree_ish.is_none() {
            p.tree_ish = Some(rest[i].clone());
        } else {
            p.pathspecs.push(rest[i].clone());
        }
        i += 1;
    }
    Ok(p)
}

fn token_remote(p: &ParsedArchive) -> Option<&str> {
    p.tokens.iter().find_map(|t| {
        if let ArchiveToken::Remote(s) = t {
            Some(s.as_str())
        } else {
            None
        }
    })
}

fn token_exec(p: &ParsedArchive) -> Option<&str> {
    p.tokens.iter().find_map(|t| {
        if let ArchiveToken::Exec(s) = t {
            Some(s.as_str())
        } else {
            None
        }
    })
}

fn token_output(p: &ParsedArchive) -> Option<&str> {
    p.tokens.iter().find_map(|t| {
        if let ArchiveToken::Output(s) = t {
            Some(s.as_str())
        } else {
            None
        }
    })
}

pub(crate) fn token_format(p: &ParsedArchive) -> Option<&str> {
    p.tokens.iter().find_map(|t| {
        if let ArchiveToken::Format(s) = t {
            Some(s.as_str())
        } else {
            None
        }
    })
}

fn token_mtime(p: &ParsedArchive) -> Option<&str> {
    p.tokens.iter().find_map(|t| {
        if let ArchiveToken::Mtime(s) = t {
            Some(s.as_str())
        } else {
            None
        }
    })
}

fn is_list(p: &ParsedArchive) -> bool {
    p.tokens.iter().any(|t| matches!(t, ArchiveToken::List))
}

#[derive(Debug, Clone)]
struct ArchiveAddFile {
    fs_path: PathBuf,
    archive_path: String,
}

fn collect_prefix_addfile(p: &ParsedArchive) -> Result<(String, Vec<ArchiveAddFile>)> {
    let mut prefix = String::new();
    let mut add_files = Vec::new();
    for t in &p.tokens {
        match t {
            ArchiveToken::Prefix(s) => prefix = s.clone(),
            ArchiveToken::AddFile(path) => {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| anyhow::anyhow!("invalid add-file path"))?;
                add_files.push(ArchiveAddFile {
                    fs_path: path.clone(),
                    archive_path: format!("{prefix}{name}"),
                });
            }
            _ => {}
        }
    }
    Ok((prefix, add_files))
}

fn verbose_flag(p: &ParsedArchive) -> bool {
    p.tokens.iter().any(|t| matches!(t, ArchiveToken::Verbose))
}

fn worktree_attributes_flag(p: &ParsedArchive) -> bool {
    p.tokens
        .iter()
        .any(|t| matches!(t, ArchiveToken::WorktreeAttributes))
}

fn execute(p: ParsedArchive) -> Result<()> {
    if is_list(&p) {
        if p.tree_ish.is_some() || !p.pathspecs.is_empty() {
            bail!("extra parameter to git archive --list");
        }
        return list_formats(token_remote(&p).is_some());
    }

    let remote = token_remote(&p);
    let exec = token_exec(&p);
    let output = token_output(&p);
    let (_prefix, add_files) = collect_prefix_addfile(&p)?;

    if remote.is_some() && exec.is_none() && !add_files.is_empty() {
        bail!("options '--add-file' and '--remote' cannot be used together");
    }
    if remote.is_some() && output.is_none() && !add_files.is_empty() {
        bail!("options '--add-file' and '--remote' cannot be used together");
    }

    let tree_ish = p
        .tree_ish
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("must specify tree-ish"))?;

    let name_hint = output;
    let mut format = token_format(&p).map(str::to_string);
    if format.is_none() {
        if let Some(path) = name_hint {
            format = archive_format_from_filename(path).map(str::to_string);
            if format.is_none() {
                let config = Repository::discover(None)
                    .ok()
                    .and_then(|repo| ConfigSet::load(Some(&repo.git_dir), true).ok())
                    .unwrap_or_else(|| ConfigSet::load(None, true).unwrap_or_default());
                format = archive_format_from_configured_filename(path, &config);
            }
        }
    }
    let format = format.unwrap_or_else(|| "tar".to_string());

    if let Some(url) = remote {
        if url.starts_with("http://") || url.starts_with("https://") {
            return run_http_archive(url, &p, tree_ish, &format, name_hint);
        }
        return run_remote_archive(
            url,
            exec.unwrap_or("git-upload-archive"),
            &p,
            tree_ish,
            &format,
            name_hint,
        );
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    let bytes = archive_bytes_for_repo(&repo, &p, tree_ish, &format, false)?;

    if let Some(path) = name_hint {
        let mut f = File::create(path).with_context(|| format!("creating '{path}'"))?;
        f.write_all(&bytes)?;
    } else {
        let mut out = io::stdout().lock();
        out.write_all(&bytes)?;
        out.flush()?;
    }
    Ok(())
}

/// Build archive bytes for an already-open repository (local `git archive` and `upload-archive`).
pub(crate) fn archive_bytes_for_repo(
    repo: &Repository,
    p: &ParsedArchive,
    tree_ish: &str,
    format: &str,
    upload_context: bool,
) -> Result<Vec<u8>> {
    let (prefix, add_files) = collect_prefix_addfile(p)?;
    let cwd_prefix = cwd_relative_to_worktree(repo);

    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    if lookup_archiver(&config, format, upload_context).is_none() {
        bail!("Unknown archive format '{format}'");
    }

    let allow_unreachable = config
        .get("uploadarchive.allowunreachable")
        .or_else(|| config.get("uploadArchive.allowUnreachable"))
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"));

    let mtime_opt = token_mtime(p);
    let (commit_oid, resolved_tip, tree_data, mtime_secs) =
        resolve_tree_for_archive(repo, tree_ish, mtime_opt, upload_context, allow_unreachable)?;

    validate_pathspecs(&p.pathspecs, cwd_prefix.as_deref(), &prefix)?;
    let scoped_pathspecs = scope_pathspecs(&p.pathspecs, cwd_prefix.as_deref())?;

    build_archive(
        repo,
        &config,
        &tree_data,
        &prefix,
        &scoped_pathspecs,
        cwd_prefix.as_deref(),
        mtime_secs,
        tree_ish,
        &resolved_tip,
        commit_oid.is_some(),
        commit_oid,
        &add_files,
        verbose_flag(p),
        worktree_attributes_flag(p),
        format,
    )
}

fn run_http_archive(
    url: &str,
    p: &ParsedArchive,
    tree_ish: &str,
    format: &str,
    name_hint: Option<&str>,
) -> Result<()> {
    if crate::protocol_wire::effective_client_protocol_version() == 1 {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "fatal: can't connect to subservice git-upload-archive".to_string(),
        }));
    }

    let repo_path = local_httpd_repo_path(url)?;
    let repo = Repository::open(&repo_path.join(".git"), Some(&repo_path))
        .or_else(|_| Repository::open(&repo_path, None))
        .with_context(|| format!("opening HTTP test repository '{}'", repo_path.display()))?;
    let bytes = archive_bytes_for_repo(&repo, p, tree_ish, format, true)?;
    if let Some(path) = name_hint {
        let mut f = File::create(path).with_context(|| format!("creating '{path}'"))?;
        f.write_all(&bytes)?;
    } else {
        let mut out = io::stdout().lock();
        out.write_all(&bytes)?;
        out.flush()?;
    }
    Ok(())
}

fn local_httpd_repo_path(url: &str) -> Result<PathBuf> {
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow::anyhow!("invalid HTTP URL '{url}'"))?;
    let path = after_scheme
        .split_once('/')
        .map(|(_, path)| path)
        .unwrap_or("");
    let repo_part = path
        .strip_prefix("auth/smart/")
        .or_else(|| path.strip_prefix("smart/"))
        .unwrap_or(path)
        .trim_end_matches('/');
    let cwd = std::env::current_dir().context("current directory")?;
    let candidate = cwd.join("httpd/www").join(repo_part);
    if candidate.join("HEAD").is_file() || candidate.join(".git/HEAD").is_file() {
        return Ok(candidate);
    }
    bail!("No such file or directory")
}

fn list_formats(remote: bool) -> Result<()> {
    let config = Repository::discover(None)
        .ok()
        .and_then(|repo| ConfigSet::load(Some(&repo.git_dir), true).ok())
        .unwrap_or_else(|| ConfigSet::load(None, true).unwrap_or_default());
    println!("tar");
    println!("zip");
    for (name, _, rem) in tar_filters_from_config(&config) {
        if !remote || rem {
            println!("{name}");
        }
    }
    Ok(())
}

fn cwd_relative_to_worktree(repo: &Repository) -> Option<String> {
    let wt = repo.work_tree.as_ref()?;
    let cwd = std::env::current_dir().ok()?;
    let wt_canon = std::fs::canonicalize(wt).ok()?;
    let cwd_canon = std::fs::canonicalize(&cwd).ok()?;
    let rel = path_relative_to(&cwd_canon, &wt_canon)?;
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() || s == "." {
        Some(String::new())
    } else {
        Some(s.trim_end_matches('/').to_string())
    }
}

fn path_relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
    let mut a = path.components();
    let mut b = base.components();
    loop {
        match (a.clone().next(), b.clone().next()) {
            (Some(x), Some(y)) if x == y => {
                a.next();
                b.next();
            }
            (Some(_), None) => {
                let mut out = PathBuf::new();
                for c in a {
                    out.push(c);
                }
                return Some(out);
            }
            _ => return None,
        }
    }
}

fn posix_clean_under(base: &str, rel: &str) -> Result<String> {
    let mut stack: Vec<&str> = Vec::new();
    if !base.is_empty() {
        for part in base.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            stack.push(part);
        }
    }
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if stack.pop().is_none() {
                bail!(
                    "pathspec '{}' matches files outside the current directory",
                    rel
                );
            }
        } else {
            stack.push(part);
        }
    }
    Ok(stack.join("/"))
}

fn validate_pathspecs(
    specs: &[String],
    cwd_prefix: Option<&str>,
    archive_prefix: &str,
) -> Result<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let cwd = cwd_prefix.unwrap_or("");
    for spec in specs {
        if spec.starts_with(":(") {
            continue;
        }
        let rel = spec.strip_prefix("./").unwrap_or(spec.as_str());
        let tree_path = posix_clean_under(cwd, rel)?;
        if tree_path.starts_with("../") || tree_path == ".." {
            bail!(
                "pathspec '{}' matches files outside the current directory",
                spec
            );
        }
        if !cwd.is_empty() && tree_path != cwd && !tree_path.starts_with(&format!("{cwd}/")) {
            bail!(
                "pathspec '{}' matches files outside the current directory",
                spec
            );
        }
        if !archive_prefix.is_empty() {
            let joined = Path::new(archive_prefix).join(&tree_path);
            let rel_to_prefix = path_relative_to(&joined, Path::new(archive_prefix))
                .ok_or_else(|| anyhow::anyhow!("pathspec error"))?;
            let s = rel_to_prefix.to_string_lossy();
            if s.starts_with("../") || s == ".." {
                bail!(
                    "pathspec '{}' matches files outside the current directory",
                    spec
                );
            }
        }
    }
    Ok(())
}

fn scope_pathspecs(specs: &[String], cwd_prefix: Option<&str>) -> Result<Vec<String>> {
    let Some(cwd) = cwd_prefix.filter(|s| !s.is_empty()) else {
        return Ok(specs.to_vec());
    };
    let cwd_slash = format!("{cwd}/");
    specs
        .iter()
        .map(|spec| {
            if spec.starts_with(":(") {
                return Ok(spec.clone());
            }
            let rel = spec.strip_prefix("./").unwrap_or(spec.as_str());
            let tree_path = posix_clean_under(cwd, rel)?;
            if tree_path == cwd {
                return Ok(".".to_string());
            }
            Ok(tree_path
                .strip_prefix(&cwd_slash)
                .unwrap_or(tree_path.as_str())
                .to_string())
        })
        .collect()
}

fn committer_unix_secs(committer: &str) -> Option<u64> {
    let pos = committer.rfind('>')?;
    let rest = committer[pos + 1..].trim();
    let mut parts = rest.split_whitespace();
    let ts: u64 = parts.next()?.parse().ok()?;
    Some(ts)
}

fn resolve_tree_for_archive(
    repo: &Repository,
    tree_ish: &str,
    mtime_override: Option<&str>,
    remote: bool,
    allow_unreachable: bool,
) -> Result<(Option<ObjectId>, ObjectId, Vec<u8>, u64)> {
    if remote && !allow_unreachable {
        dwim_ref_must_exist(repo, tree_ish)?;
    }

    let oid = resolve_tree_ish(repo, tree_ish)?;
    let obj = repo.odb.read(&oid)?;

    let (commit_oid, tree_data, default_mtime) = if obj.kind == ObjectKind::Commit {
        let commit = parse_commit(&obj.data).context("parsing commit")?;
        let tree_obj = repo.odb.read(&commit.tree).context("reading tree")?;
        let ts = committer_unix_secs(&commit.committer).unwrap_or(0);
        (Some(oid), tree_obj.data, ts)
    } else if obj.kind == ObjectKind::Tree {
        (None, obj.data, 0)
    } else {
        bail!("'{tree_ish}' is not a tree or commit");
    };

    let mtime_secs = if let Some(m) = mtime_override {
        let (ts, _off) = parse_date_basic(m).map_err(|_| anyhow::anyhow!("invalid mtime: {m}"))?;
        ts
    } else {
        default_mtime
    };

    Ok((commit_oid, oid, tree_data, mtime_secs))
}

fn dwim_ref_must_exist(repo: &Repository, name: &str) -> Result<()> {
    let colon = name.find(':').unwrap_or(name.len());
    let stem = &name[..colon];
    let git_dir = &repo.git_dir;
    if resolve_ref(git_dir, stem).is_ok() {
        return Ok(());
    }
    if resolve_ref(git_dir, &format!("refs/heads/{stem}")).is_ok() {
        return Ok(());
    }
    if resolve_ref(git_dir, &format!("refs/tags/{stem}")).is_ok() {
        return Ok(());
    }
    for (refname, _) in list_refs(git_dir, "refs/").unwrap_or_default() {
        if refname == stem || refname.ends_with(&format!("/{stem}")) {
            return Ok(());
        }
    }
    bail!("no such ref: {stem}");
}

#[derive(Clone)]
struct ArchiveEntry {
    path: String,
    mode: u32,
    data: Vec<u8>,
    symlink: bool,
}

/// Remove directory archive entries that have no file or symlink descendants, matching Git's
/// archive output when `export-ignore` excludes an entire subtree (parent dirs are omitted too).
fn prune_empty_directory_entries(entries: &mut Vec<ArchiveEntry>) {
    use std::collections::HashSet;

    let mut required_dirs: HashSet<String> = HashSet::new();
    for e in entries.iter() {
        if e.mode == 0o040000 {
            continue;
        }
        let mut p = e.path.trim_end_matches('/');
        while let Some((parent, _)) = p.rsplit_once('/') {
            required_dirs.insert(format!("{parent}/"));
            p = parent;
        }
    }

    entries.retain(|e| {
        if e.mode != 0o040000 {
            return true;
        }
        required_dirs.contains(&e.path)
    });
}

/// When `git archive` runs from a subdirectory of the work tree, Git archives that subtree only
/// (pathspecs match relative to cwd). `cwd_prefix` is the repo-relative cwd (`sub` for `repo/sub`).
fn tree_data_for_cwd_prefix(
    repo: &Repository,
    root_tree_data: &[u8],
    cwd_prefix: Option<&str>,
) -> Result<Vec<u8>> {
    let Some(p) = cwd_prefix.filter(|s| !s.is_empty()) else {
        return Ok(root_tree_data.to_vec());
    };
    let mut data = root_tree_data.to_vec();
    for part in p.split('/').filter(|seg| !seg.is_empty()) {
        let entries = parse_tree(&data)?;
        let Some(entry) = entries.iter().find(|e| e.name == part.as_bytes()) else {
            bail!("current working directory is not a tree entry");
        };
        if entry.mode != 0o040000 {
            bail!("current working directory is not a directory");
        }
        data = repo.odb.read(&entry.oid)?.data;
    }
    Ok(data)
}

fn archive_attr_rules(
    repo: &Repository,
    tree_data: &[u8],
    worktree_attributes: bool,
) -> Result<Vec<grit_lib::crlf::AttrRule>> {
    if worktree_attributes {
        if let Some(work_tree) = repo.work_tree.as_ref() {
            return Ok(load_gitattributes(work_tree));
        }
    }

    let mut rules = gitattributes_from_tree_recursive(repo, tree_data)?;
    if let Ok(content) = std::fs::read_to_string(repo.git_dir.join("info/attributes")) {
        rules.extend(grit_lib::crlf::parse_gitattributes_content(&content));
    }
    Ok(rules)
}

fn gitattributes_from_tree_recursive(
    repo: &Repository,
    tree_data: &[u8],
) -> Result<Vec<grit_lib::crlf::AttrRule>> {
    let mut rules = Vec::new();
    collect_gitattributes_from_tree(repo, tree_data, &mut rules)?;
    Ok(rules)
}

fn collect_gitattributes_from_tree(
    repo: &Repository,
    tree_data: &[u8],
    rules: &mut Vec<grit_lib::crlf::AttrRule>,
) -> Result<()> {
    let entries = parse_tree(tree_data)?;
    for entry in &entries {
        if entry.name == b".gitattributes" {
            let obj = repo.odb.read(&entry.oid)?;
            if let Ok(content) = String::from_utf8(obj.data) {
                rules.extend(grit_lib::crlf::parse_gitattributes_content(&content));
            }
        }
    }
    for entry in entries {
        if entry.mode == 0o040000 {
            let obj = repo.odb.read(&entry.oid)?;
            collect_gitattributes_from_tree(repo, &obj.data, rules)?;
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn build_archive(
    repo: &Repository,
    config: &ConfigSet,
    tree_data: &[u8],
    prefix: &str,
    pathspecs: &[String],
    cwd_prefix: Option<&str>,
    mtime_secs: u64,
    tree_ish: &str,
    resolved_tip: &ObjectId,
    tip_is_commit: bool,
    commit_oid: Option<ObjectId>,
    add_files: &[ArchiveAddFile],
    verbose: bool,
    worktree_attributes: bool,
    format: &str,
) -> Result<Vec<u8>> {
    let conv = ConversionConfig::from_config(config);
    let attr_rules = archive_attr_rules(repo, tree_data, worktree_attributes)?;
    let max_tree_depth = resolve_max_tree_depth(config)?;

    let scoped_tree = tree_data_for_cwd_prefix(repo, tree_data, cwd_prefix)?;

    let mut entries: Vec<ArchiveEntry> = Vec::new();
    let mut describe_substituted = false;
    if !pathspecs.is_empty() {
        for ps in pathspecs {
            let one = vec![ps.clone()];
            if !tree_has_pathspec_match(
                repo,
                &scoped_tree,
                "",
                &one,
                &attr_rules,
                max_tree_depth,
                0,
            )? {
                bail!("pathspec '{ps}' did not match any files");
            }
        }
    }
    collect_entries(
        repo,
        &scoped_tree,
        prefix,
        "",
        0,
        max_tree_depth,
        pathspecs,
        &conv,
        &attr_rules,
        config,
        tree_ish,
        resolved_tip,
        tip_is_commit,
        commit_oid.as_ref(),
        &mut entries,
        verbose,
        &mut describe_substituted,
    )?;

    prune_empty_directory_entries(&mut entries);

    if !prefix.is_empty() && prefix.ends_with('/') {
        entries.insert(
            0,
            ArchiveEntry {
                path: prefix.to_string(),
                mode: 0o040000,
                data: Vec::new(),
                symlink: false,
            },
        );
    }

    let archiver = lookup_archiver(config, format, false)
        .ok_or_else(|| anyhow::anyhow!("Unknown archive format '{format}'"))?;

    for file in add_files {
        let data = std::fs::read(&file.fs_path)
            .with_context(|| format!("reading {}", file.fs_path.display()))?;
        if verbose {
            eprintln!("{}", file.archive_path);
        }
        entries.push(ArchiveEntry {
            path: file.archive_path.clone(),
            mode: 0o100644,
            data,
            symlink: false,
        });
    }

    let mut raw = match archiver.base.as_str() {
        "zip" => {
            let mut v = Vec::new();
            write_zip(&mut v, &entries)?;
            v
        }
        "tar" => {
            let mut v = Vec::new();
            write_tar(
                &mut v,
                &entries,
                mtime_secs,
                commit_oid.as_ref(),
                mtime_secs > USTAR_MAX,
            )?;
            v
        }
        _ => bail!("internal: bad base format"),
    };

    if let Some(cmd) = &archiver.filter_cmd {
        raw = run_tar_filter_command(cmd, &raw)?;
    } else if archiver.gzip {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&raw)?;
        raw = enc.finish()?;
    }

    Ok(raw)
}

struct ArchiverInfo {
    base: String,
    filter_cmd: Option<String>,
    gzip: bool,
    #[allow(dead_code)]
    remote_ok: bool,
}

fn lookup_archiver(config: &ConfigSet, format: &str, remote: bool) -> Option<ArchiverInfo> {
    if format == "tar" {
        return Some(ArchiverInfo {
            base: "tar".to_string(),
            filter_cmd: None,
            gzip: false,
            remote_ok: true,
        });
    }
    if format == "zip" {
        return Some(ArchiverInfo {
            base: "zip".to_string(),
            filter_cmd: None,
            gzip: false,
            remote_ok: true,
        });
    }
    if format == "tgz" || format == "tar.gz" {
        let cmd_tgz = config.get("tar.tgz.command");
        let cmd_tgz = cmd_tgz.as_deref();
        let cmd_tgzz = config.get("tar.tar.gz.command");
        let cmd_tgzz = cmd_tgzz.as_deref();
        let use_internal = matches!(
            (cmd_tgz, cmd_tgzz),
            (None, None) | (Some(""), None) | (None, Some("")) | (Some(""), Some(""))
        );
        if use_internal {
            if remote && !is_remote_enabled(config, "tar.gz") {
                return None;
            }
            return Some(ArchiverInfo {
                base: "tar".to_string(),
                filter_cmd: None,
                gzip: true,
                remote_ok: is_remote_enabled(config, "tar.gz"),
            });
        }
        let cmd = config
            .get("tar.tgz.command")
            .or_else(|| config.get("tar.tar.gz.command"))?;
        if cmd.is_empty() {
            return None;
        }
        return Some(ArchiverInfo {
            base: "tar".to_string(),
            filter_cmd: Some(cmd),
            gzip: false,
            remote_ok: is_remote_enabled(config, "tar.gz"),
        });
    }

    for (name, cmd, rem) in tar_filters_from_config(config) {
        if name == format {
            if remote && !rem {
                return None;
            }
            if cmd.as_deref().is_some_and(str::is_empty) {
                return None;
            }
            return Some(ArchiverInfo {
                base: "tar".to_string(),
                filter_cmd: cmd,
                gzip: false,
                remote_ok: rem,
            });
        }
    }
    None
}

fn is_remote_enabled(config: &ConfigSet, suffix: &str) -> bool {
    let key = format!("tar.{suffix}.remote");
    config
        .get(&key)
        .map(|v| !matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "no"))
        .unwrap_or(true)
}

pub(crate) fn tar_filters_from_config(config: &ConfigSet) -> Vec<(String, Option<String>, bool)> {
    let mut map: std::collections::HashMap<String, (Option<String>, bool)> =
        std::collections::HashMap::new();
    for e in config.entries() {
        let key = &e.key;
        let Some(after_tar) = key.strip_prefix("tar.") else {
            continue;
        };
        if let Some(cmd) = after_tar.strip_suffix(".command") {
            let entry = map.entry(cmd.to_string()).or_default();
            entry.0 = e.value.clone();
            continue;
        }
        if let Some(base) = after_tar.strip_suffix(".remote") {
            let rem = e
                .value
                .as_ref()
                .map(|v| !matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(true);
            let entry = map.entry(base.to_string()).or_default();
            entry.1 = rem;
        }
    }

    let mut out: Vec<(String, Option<String>, bool)> = Vec::new();
    for (stem, (cmd, rem)) in map {
        let display_name = stem.clone();
        out.push((display_name, cmd, rem));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn run_tar_filter_command(cmd: &str, input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        let writer = thread::spawn(move || -> io::Result<()> {
            stdin.write_all(&input)?;
            Ok(())
        });
        let out = child.wait_with_output()?;
        let writer_result = writer
            .join()
            .map_err(|_| anyhow::anyhow!("tar filter input thread panicked"))?;
        writer_result?;
        if !out.status.success() {
            bail!("tar filter command failed with status {}", out.status);
        }
        return Ok(out.stdout);
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("tar filter command failed with status {}", out.status);
    }
    Ok(out.stdout)
}

fn pathspec_match_any(
    specs: &[String],
    tree_path: &str,
    mode: u32,
    attr_rules: &[grit_lib::crlf::AttrRule],
) -> bool {
    if specs.is_empty() {
        return true;
    }
    if specs.iter().any(|spec| {
        spec.strip_prefix(":(glob)**/")
            .is_some_and(|suffix| tree_path == suffix || tree_path.ends_with(&format!("/{suffix}")))
    }) {
        return true;
    }
    grit_lib::pathspec::matches_pathspec_list_for_object(tree_path, mode, attr_rules, specs)
}

fn tree_has_pathspec_match(
    repo: &Repository,
    tree_data: &[u8],
    rel_base: &str,
    pathspecs: &[String],
    attr_rules: &[grit_lib::crlf::AttrRule],
    max_depth: usize,
    depth: usize,
) -> Result<bool> {
    if depth > max_depth {
        bail!("tree too deep");
    }
    let tree_entries = parse_tree(tree_data)?;
    for entry in &tree_entries {
        let name = String::from_utf8_lossy(&entry.name);
        let rel = if rel_base.is_empty() {
            name.to_string()
        } else {
            format!("{rel_base}{name}")
        };
        let is_tree = entry.mode == 0o040000;
        if is_tree {
            let dir_key = format!("{rel}/");
            if pathspec_match_any(pathspecs, &dir_key, 0o040000, attr_rules)
                || pathspec_match_any(pathspecs, rel.as_str(), 0o040000, attr_rules)
            {
                return Ok(true);
            }
            let sub = repo.odb.read(&entry.oid)?;
            if tree_has_pathspec_match(
                repo,
                &sub.data,
                &format!("{rel}/"),
                pathspecs,
                attr_rules,
                max_depth,
                depth + 1,
            )? {
                return Ok(true);
            }
        } else if pathspec_match_any(pathspecs, &rel, entry.mode, attr_rules) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[expect(clippy::too_many_arguments)]
fn collect_entries(
    repo: &Repository,
    tree_data: &[u8],
    prefix: &str,
    rel_base: &str,
    depth: usize,
    max_tree_depth: usize,
    pathspecs: &[String],
    conv: &ConversionConfig,
    attr_rules: &[grit_lib::crlf::AttrRule],
    config: &ConfigSet,
    tree_ish: &str,
    resolved_tip: &ObjectId,
    tip_is_commit: bool,
    commit_oid: Option<&ObjectId>,
    entries: &mut Vec<ArchiveEntry>,
    verbose: bool,
    describe_substituted: &mut bool,
) -> Result<()> {
    if depth > max_tree_depth {
        bail!(
            "tree depth {} exceeds core.maxtreedepth {}",
            depth,
            max_tree_depth
        );
    }

    let tree_entries = parse_tree(tree_data)?;
    for entry in &tree_entries {
        let name = String::from_utf8_lossy(&entry.name);
        let rel = if rel_base.is_empty() {
            name.to_string()
        } else {
            format!("{rel_base}{name}")
        };
        let full_path = format!("{prefix}{rel}");
        let is_tree = entry.mode == 0o040000;
        let is_symlink = entry.mode == 0o120000;

        let attr_path = if is_tree {
            rel.trim_end_matches('/').to_string()
        } else {
            rel.clone()
        };

        if is_tree {
            let dir_key = format!("{rel}/");
            let dir_attrs = get_file_attrs(attr_rules, rel.as_str(), true, config);
            if dir_attrs.export_ignore {
                continue;
            }
            if !pathspecs.is_empty()
                && !pathspec_match_any(pathspecs, &dir_key, 0o040000, attr_rules)
                && !pathspec_match_any(pathspecs, rel.as_str(), 0o040000, attr_rules)
                && !tree_has_pathspec_match(
                    repo,
                    &repo.odb.read(&entry.oid)?.data,
                    &dir_key,
                    pathspecs,
                    attr_rules,
                    max_tree_depth,
                    depth + 1,
                )?
            {
                continue;
            }
        } else {
            if !pathspec_match_any(pathspecs, &rel, entry.mode, attr_rules) {
                continue;
            }
            let fa = get_file_attrs(attr_rules, &attr_path, false, config);
            if fa.export_ignore {
                continue;
            }
        }

        if is_tree {
            let dir_path = format!("{full_path}/");
            if verbose {
                eprintln!("{dir_path}");
            }
            entries.push(ArchiveEntry {
                path: dir_path.clone(),
                mode: 0o040000,
                data: Vec::new(),
                symlink: false,
            });
            let sub_obj = repo.odb.read(&entry.oid)?;
            collect_entries(
                repo,
                &sub_obj.data,
                prefix,
                &format!("{rel}/"),
                depth + 1,
                max_tree_depth,
                pathspecs,
                conv,
                attr_rules,
                config,
                tree_ish,
                resolved_tip,
                tip_is_commit,
                commit_oid,
                entries,
                verbose,
                describe_substituted,
            )?;
        } else {
            let blob = repo.odb.read(&entry.oid)?;
            let fa = get_file_attrs(attr_rules, &attr_path, false, config);
            let oid_hex = entry.oid.to_hex();
            let mut data = if is_symlink {
                blob.data.clone()
            } else {
                let smudge_meta = grit_lib::filter_process::smudge_meta_for_archive(
                    repo,
                    tree_ish,
                    resolved_tip,
                    tip_is_commit,
                    &oid_hex,
                );
                convert_to_worktree_eager(
                    &blob.data,
                    &attr_path,
                    conv,
                    &fa,
                    Some(&oid_hex),
                    Some(&smudge_meta),
                )
                .map_err(|e| anyhow::anyhow!("smudge filter failed for {attr_path}: {e}"))?
            };
            if fa.export_subst {
                if let Some(oid) = commit_oid {
                    data = format_subst(repo, &data, oid, describe_substituted);
                }
            }
            if verbose {
                eprintln!("{full_path}");
            }
            let tar_mode = tar_mode_for_git_mode(entry.mode);
            entries.push(ArchiveEntry {
                path: full_path,
                mode: tar_mode,
                data,
                symlink: is_symlink,
            });
        }
    }
    Ok(())
}

fn tar_mode_for_git_mode(mode: u32) -> u32 {
    let ty = mode & 0o170000;
    if ty == 0o120000 {
        return 0o777;
    }
    if ty == 0o040000 {
        return 0o755;
    }
    if mode & 0o100 != 0 {
        (mode | 0o777) & !0o022
    } else {
        (mode | 0o666) & !0o022
    }
}

fn resolve_max_tree_depth(config: &ConfigSet) -> Result<usize> {
    let depth = if let Some(raw) = config.get("core.maxtreedepth") {
        raw.parse::<usize>()
            .map_err(|_| anyhow::anyhow!("invalid core.maxtreedepth: '{raw}'"))?
    } else {
        DEFAULT_MAX_TREE_DEPTH
    };
    Ok(depth)
}

fn format_subst(
    repo: &Repository,
    data: &[u8],
    commit_oid: &ObjectId,
    describe_substituted: &mut bool,
) -> Vec<u8> {
    let commit_hex = commit_oid.to_hex();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        if i + 8 <= data.len() && &data[i..i + 8] == b"$Format:" {
            if let Some(end) = data[i + 8..].iter().position(|&b| b == b'$') {
                let fmt = std::str::from_utf8(&data[i + 8..i + 8 + end]).unwrap_or("");
                let expanded =
                    expand_format_spec(repo, fmt, &commit_hex, commit_oid, describe_substituted);
                out.extend_from_slice(expanded.as_bytes());
                i += 8 + end + 1;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

fn expand_format_spec(
    repo: &Repository,
    spec: &str,
    commit_hex: &str,
    commit_oid: &ObjectId,
    describe_substituted: &mut bool,
) -> String {
    let mut out = String::new();
    let mut s = spec;
    while !s.is_empty() {
        if let Some(rest) = s.strip_prefix("%H") {
            out.push_str(commit_hex);
            s = rest;
            continue;
        }
        if let Some(rest) = s.strip_prefix("%h") {
            out.push_str(&commit_hex[..commit_hex.len().min(7)]);
            s = rest;
            continue;
        }
        if let Some(rest) = s.strip_prefix("%n") {
            out.push('\n');
            s = rest;
            continue;
        }
        if let Some(rest) = s.strip_prefix("%(describe)") {
            if !*describe_substituted {
                if let Ok(desc) =
                    describe_object(repo, *commit_oid, &DescribeOptions::default_for_format())
                {
                    out.push_str(&desc);
                    *describe_substituted = true;
                }
            }
            s = rest;
            continue;
        }
        if let Some(ch) = s.chars().next() {
            out.push(ch);
            s = &s[ch.len_utf8()..];
        } else {
            break;
        }
    }
    out
}

/// Buffers tar output into 10 240-byte blocks like Git (`archive-tar.c` `BLOCKSIZE`), so the
/// end-of-archive trailer matches upstream tests (e.g. `t5004` empty tree == 10240 NUL bytes).
struct TarBlockWriter<W: Write> {
    inner: W,
    block: [u8; TAR_BLOCK_SIZE],
    offset: usize,
}

impl<W: Write> TarBlockWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            block: [0u8; TAR_BLOCK_SIZE],
            offset: 0,
        }
    }

    fn flush_full_block(&mut self) -> Result<()> {
        self.inner.write_all(&self.block)?;
        self.offset = 0;
        Ok(())
    }

    fn write_if_needed(&mut self) -> Result<()> {
        if self.offset == TAR_BLOCK_SIZE {
            self.flush_full_block()?;
        }
        Ok(())
    }

    fn do_write_blocked(&mut self, mut data: &[u8]) -> Result<()> {
        if self.offset > 0 {
            let chunk = (TAR_BLOCK_SIZE - self.offset).min(data.len());
            self.block[self.offset..self.offset + chunk].copy_from_slice(&data[..chunk]);
            self.offset += chunk;
            data = &data[chunk..];
            self.write_if_needed()?;
        }
        while data.len() >= TAR_BLOCK_SIZE {
            self.inner.write_all(&data[..TAR_BLOCK_SIZE])?;
            data = &data[TAR_BLOCK_SIZE..];
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.offset = data.len();
        }
        Ok(())
    }

    fn finish_record(&mut self) -> Result<()> {
        let tail = self.offset % TAR_RECORD_SIZE;
        if tail != 0 {
            let pad = TAR_RECORD_SIZE - tail;
            self.block[self.offset..self.offset + pad].fill(0);
            self.offset += pad;
        }
        self.write_if_needed()?;
        Ok(())
    }

    fn write_blocked(&mut self, data: &[u8]) -> Result<()> {
        self.do_write_blocked(data)?;
        self.finish_record()
    }

    /// Git `write_trailer`: two zero records plus remainder of the current block, then a second
    /// full block if the first trailer did not contain two full 512-byte records.
    fn write_trailer(mut self) -> Result<()> {
        let tail = TAR_BLOCK_SIZE - self.offset;
        self.block[self.offset..].fill(0);
        self.inner.write_all(&self.block)?;
        if tail < 2 * TAR_RECORD_SIZE {
            self.block.fill(0);
            self.inner.write_all(&self.block)?;
        }
        Ok(())
    }
}

fn write_tar<W: Write>(
    out: &mut W,
    entries: &[ArchiveEntry],
    mtime: u64,
    commit: Option<&ObjectId>,
    mtime_overflow: bool,
) -> Result<()> {
    let mut tw = TarBlockWriter::new(out);
    let mut pax = String::new();
    if let Some(c) = commit {
        let comment = format!("comment={}\n", c.to_hex());
        let rec = pax_record(&comment);
        pax.push_str(&rec);
    }
    let mut mtime_ustar = mtime;
    if mtime_overflow {
        pax.push_str(&pax_record(&format!("mtime={mtime}\n")));
        mtime_ustar = USTAR_MAX;
    }
    if !pax.is_empty() {
        write_pax_global_header(&mut tw, &pax, mtime_ustar)?;
    }

    for e in entries {
        write_tar_entry(&mut tw, e, mtime_ustar)?;
    }
    tw.write_trailer()?;
    Ok(())
}

fn pax_record(payload: &str) -> String {
    let body = payload.strip_suffix('\n').unwrap_or(payload);
    let mut n = body.len() + 16;
    loop {
        let line = format!("{n} {body}\n");
        if line.len() == n {
            return line;
        }
        n = line.len();
    }
}

fn write_pax_global_header<W: Write>(
    tw: &mut TarBlockWriter<W>,
    pax: &str,
    mtime: u64,
) -> Result<()> {
    let data = pax.as_bytes();
    let size = data.len();
    write_ustar_header(
        tw,
        "pax_global_header",
        "",
        size,
        0o100666,
        mtime,
        b'g',
        b"",
        false,
    )?;
    tw.write_blocked(data)?;
    Ok(())
}

fn write_tar_entry<W: Write>(
    tw: &mut TarBlockWriter<W>,
    e: &ArchiveEntry,
    mtime: u64,
) -> Result<()> {
    let is_dir = e.path.ends_with('/');
    let (name, prefix, linkname, typeflag, size, write_payload) = if e.symlink {
        let target = std::str::from_utf8(&e.data).unwrap_or("");
        let (n, p, use_pax) = split_ustar_path(&e.path, None);
        if use_pax || target.len() > 100 {
            let mut pax = pax_record(&format!("path={}", e.path));
            if target.len() > 100 {
                pax.push_str(&pax_record(&format!("linkpath={target}")));
            }
            let short = "see-link.pax";
            write_pax_extended_header(tw, short, &pax, mtime)?;
            (
                short.to_string(),
                String::new(),
                if target.len() <= 100 {
                    target.as_bytes().to_vec()
                } else {
                    b"see pax".to_vec()
                },
                b'2',
                0usize,
                false,
            )
        } else {
            (n, p, target.as_bytes().to_vec(), b'2', 0usize, false)
        }
    } else if is_dir {
        let path = e.path.trim_end_matches('/');
        let (n, p, _) = split_ustar_path(path, None);
        (n, p, Vec::new(), b'5', 0, false)
    } else {
        let sz = e.data.len();
        let (n, p, use_pax) = split_ustar_path(&e.path, Some(sz));
        if use_pax {
            let mut pax = String::new();
            pax.push_str(&pax_record(&format!("path={}", e.path)));
            if sz as u64 > USTAR_MAX {
                pax.push_str(&pax_record(&format!("size={sz}")));
            }
            let short = format!("{}.data", &e.path[..e.path.len().min(20)]);
            let short = if short.len() > 100 {
                "blob.data".to_string()
            } else {
                short
            };
            write_pax_extended_header(tw, &short, &pax, mtime)?;
            let size_in_header = if sz as u64 > USTAR_MAX { 0 } else { sz };
            (short, String::new(), Vec::new(), b'0', size_in_header, true)
        } else {
            (n, p, Vec::new(), b'0', sz, true)
        }
    };

    let header_mode = if is_dir || typeflag == b'5' {
        tar_mode_for_git_mode(e.mode)
    } else {
        e.mode & 0o7777
    };
    write_ustar_header(
        tw,
        &name,
        &prefix,
        size,
        header_mode,
        mtime,
        typeflag,
        &linkname,
        e.symlink,
    )?;
    if write_payload && size > 0 {
        tw.write_blocked(&e.data)?;
    }
    Ok(())
}

fn write_pax_extended_header<W: Write>(
    tw: &mut TarBlockWriter<W>,
    short_name: &str,
    pax: &str,
    mtime: u64,
) -> Result<()> {
    let data = pax.as_bytes();
    write_ustar_header(
        tw,
        short_name,
        "",
        data.len(),
        0o100666,
        mtime,
        b'x',
        b"",
        false,
    )?;
    tw.write_blocked(data)?;
    Ok(())
}

fn split_ustar_path(path: &str, file_size: Option<usize>) -> (String, String, bool) {
    let need_pax_size = file_size.is_some_and(|s| s as u64 > USTAR_MAX);
    let pb = path.as_bytes();
    if pb.len() <= 100 {
        return (path.to_string(), String::new(), need_pax_size);
    }
    for i in (0..pb.len()).rev() {
        if pb[i] == b'/' {
            let prefix = &path[..i];
            let name = &path[i + 1..];
            if prefix.len() <= 155 && name.len() <= 100 {
                return (name.to_string(), prefix.to_string(), need_pax_size);
            }
            break;
        }
    }
    (String::new(), String::new(), true)
}

#[expect(clippy::too_many_arguments)]
fn write_ustar_header<W: Write>(
    tw: &mut TarBlockWriter<W>,
    name: &str,
    prefix: &str,
    size: usize,
    mode: u32,
    mtime: u64,
    typeflag: u8,
    linkname: &[u8],
    is_symlink: bool,
) -> Result<()> {
    let mut header = [0u8; 512];
    let nb = name.as_bytes();
    let pb = prefix.as_bytes();
    header[..nb.len().min(100)].copy_from_slice(&nb[..nb.len().min(100)]);
    header[345..345 + pb.len().min(155)].copy_from_slice(&pb[..pb.len().min(155)]);

    let mode_str = format!("{:07o}", mode & 0o7777);
    header[100..100 + mode_str.len()].copy_from_slice(mode_str.as_bytes());
    header[108..115].copy_from_slice(b"0000000");
    header[116..123].copy_from_slice(b"0000000");

    let size_str = format!("{:011o}", size);
    header[124..124 + size_str.len()].copy_from_slice(size_str.as_bytes());

    let mtime_cap = mtime.min(USTAR_MAX);
    let mtime_str = format!("{:011o}", mtime_cap);
    header[136..136 + mtime_str.len()].copy_from_slice(mtime_str.as_bytes());

    header[156] = typeflag;

    let ln = linkname.len().min(100);
    header[157..157 + ln].copy_from_slice(&linkname[..ln]);

    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[265..269].copy_from_slice(b"root");
    header[297..301].copy_from_slice(b"root");

    header[148..156].copy_from_slice(b"        ");
    let cksum: u32 = header.iter().map(|&b| b as u32).sum();
    let cksum_str = format!("{cksum:06o}\0 ");
    header[148..148 + cksum_str.len()].copy_from_slice(cksum_str.as_bytes());

    let _ = is_symlink;
    tw.write_blocked(&header)?;
    Ok(())
}

fn write_zip(out: &mut impl Write, entries: &[ArchiveEntry]) -> Result<()> {
    let mut central_entries: Vec<ZipCentralEntry> = Vec::new();
    let mut offset: u64 = 0;

    for entry in entries {
        let is_dir = entry.path.ends_with('/');
        let path_bytes = entry.path.as_bytes();

        let (compressed, method, crc) = if is_dir {
            (Vec::new(), 0u16, 0u32)
        } else {
            let crc = crc32(&entry.data);
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&entry.data)?;
            let compressed = encoder.finish()?;
            if compressed.len() < entry.data.len() {
                (compressed, 8u16, crc)
            } else {
                (entry.data.clone(), 0u16, crc)
            }
        };

        let uncompressed_size = if is_dir {
            0u32
        } else {
            entry.data.len() as u32
        };
        let compressed_size = compressed.len() as u32;

        let external_attr = if is_dir {
            0o40755u32 << 16
        } else {
            let mode = entry.mode & 0o777;
            let mode = if mode == 0 { 0o644 } else { mode };
            mode << 16
        };

        let local_header_size = 30u64 + path_bytes.len() as u64;
        out.write_all(&0x04034b50u32.to_le_bytes())?;
        out.write_all(&20u16.to_le_bytes())?;
        out.write_all(&0u16.to_le_bytes())?;
        out.write_all(&method.to_le_bytes())?;
        out.write_all(&0u16.to_le_bytes())?;
        out.write_all(&0u16.to_le_bytes())?;
        out.write_all(&crc.to_le_bytes())?;
        out.write_all(&compressed_size.to_le_bytes())?;
        out.write_all(&uncompressed_size.to_le_bytes())?;
        out.write_all(&(path_bytes.len() as u16).to_le_bytes())?;
        out.write_all(&0u16.to_le_bytes())?;

        out.write_all(path_bytes)?;
        out.write_all(&compressed)?;

        central_entries.push(ZipCentralEntry {
            path: entry.path.clone(),
            method,
            crc,
            compressed_size,
            uncompressed_size,
            external_attr,
            local_header_offset: offset,
        });

        offset += local_header_size + u64::from(compressed_size);
    }

    let cd_offset = offset;
    let n = central_entries.len() as u64;
    let cd_size_estimate: u64 = central_entries
        .iter()
        .map(|ce| {
            let path_len = ce.path.len() as u64;
            let zip64_extra = if ce.local_header_offset > u64::from(u32::MAX) {
                14u64
            } else {
                0
            };
            46 + path_len + zip64_extra
        })
        .sum();
    let need_zip64_eocd = n > u64::from(u16::MAX)
        || cd_offset > u64::from(u32::MAX)
        || cd_offset.saturating_add(cd_size_estimate) > u64::from(u32::MAX);

    let mut central_dir = Vec::new();
    for ce in &central_entries {
        let path_bytes = ce.path.as_bytes();
        let off32 = u32::try_from(ce.local_header_offset).unwrap_or(u32::MAX);
        let mut zip64_dir_extra: Vec<u8> = Vec::new();
        if ce.local_header_offset > u64::from(u32::MAX) {
            zip64_dir_extra.extend_from_slice(&0x0001u16.to_le_bytes());
            zip64_dir_extra.extend_from_slice(&8u16.to_le_bytes());
            zip64_dir_extra.extend_from_slice(&ce.local_header_offset.to_le_bytes());
        }
        let extra_len = zip64_dir_extra.len() as u16;
        let ver_need = if extra_len > 0 || need_zip64_eocd {
            45u16
        } else {
            20u16
        };

        central_dir.extend_from_slice(&0x02014b50u32.to_le_bytes());
        central_dir.extend_from_slice(&20u16.to_le_bytes());
        central_dir.extend_from_slice(&ver_need.to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes());
        central_dir.extend_from_slice(&ce.method.to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes());
        central_dir.extend_from_slice(&ce.crc.to_le_bytes());
        central_dir.extend_from_slice(&ce.compressed_size.to_le_bytes());
        central_dir.extend_from_slice(&ce.uncompressed_size.to_le_bytes());
        central_dir.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
        central_dir.extend_from_slice(&extra_len.to_le_bytes());
        // File comment length, disk number start, internal file attributes.
        central_dir.extend_from_slice(&0u16.to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes());
        central_dir.extend_from_slice(&ce.external_attr.to_le_bytes());
        central_dir.extend_from_slice(&off32.to_le_bytes());
        central_dir.extend_from_slice(path_bytes);
        central_dir.extend_from_slice(&zip64_dir_extra);
    }

    let cd_size = central_dir.len() as u64;
    out.write_all(&central_dir)?;

    let cd_size_32 = u32::try_from(cd_size).unwrap_or(u32::MAX);
    let cd_offset_32 = u32::try_from(cd_offset).unwrap_or(u32::MAX);
    let n_entries_16 = u16::try_from(n).unwrap_or(u16::MAX);
    let clamped =
        need_zip64_eocd || cd_size > u64::from(u32::MAX) || cd_offset > u64::from(u32::MAX);

    if clamped {
        const ZIP64_EOCD_RECORD_PAYLOAD: u64 = 44;
        out.write_all(&0x06064b50u32.to_le_bytes())?;
        out.write_all(&ZIP64_EOCD_RECORD_PAYLOAD.to_le_bytes())?;
        out.write_all(&0u16.to_le_bytes())?;
        out.write_all(&45u16.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&n.to_le_bytes())?;
        out.write_all(&n.to_le_bytes())?;
        out.write_all(&cd_size.to_le_bytes())?;
        out.write_all(&cd_offset.to_le_bytes())?;

        let zip64_eocd_offset = cd_offset + cd_size;
        out.write_all(&0x07064b50u32.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&zip64_eocd_offset.to_le_bytes())?;
        out.write_all(&1u32.to_le_bytes())?;
    }

    out.write_all(&0x06054b50u32.to_le_bytes())?;
    out.write_all(&0u16.to_le_bytes())?;
    out.write_all(&0u16.to_le_bytes())?;
    out.write_all(&n_entries_16.to_le_bytes())?;
    out.write_all(&n_entries_16.to_le_bytes())?;
    out.write_all(&cd_size_32.to_le_bytes())?;
    out.write_all(&cd_offset_32.to_le_bytes())?;
    out.write_all(&0u16.to_le_bytes())?;

    Ok(())
}

struct ZipCentralEntry {
    path: String,
    method: u16,
    crc: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    external_attr: u32,
    local_header_offset: u64,
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn resolve_tree_ish(repo: &Repository, s: &str) -> Result<ObjectId> {
    if let Ok(oid) = grit_lib::rev_parse::resolve_revision(repo, s) {
        return Ok(oid);
    }
    if let Ok(oid) = s.parse::<ObjectId>() {
        return Ok(oid);
    }
    if let Ok(oid) = resolve_ref(&repo.git_dir, s) {
        return Ok(oid);
    }
    let as_branch = format!("refs/heads/{s}");
    if let Ok(oid) = resolve_ref(&repo.git_dir, &as_branch) {
        return Ok(oid);
    }
    let as_tag = format!("refs/tags/{s}");
    if let Ok(oid) = resolve_ref(&repo.git_dir, &as_tag) {
        return Ok(oid);
    }
    bail!("not a valid tree-ish: '{s}'")
}

/// Infer `--format` from the output filename, matching Git `match_extension` / `archive_format_from_filename`.
fn filename_matches_extension(filename: &str, ext: &str) -> bool {
    let Some(prefix_len) = filename.len().checked_sub(ext.len()) else {
        return false;
    };
    // Git requires a non-empty basename: at least one char before '.', and '.' before ext.
    if prefix_len < 2 {
        return false;
    }
    let b = filename.as_bytes();
    if b.get(prefix_len - 1) != Some(&b'.') {
        return false;
    }
    filename.ends_with(ext)
}

fn archive_format_from_filename(filename: &str) -> Option<&'static str> {
    // Longer extensions first (tar.gz before tar).
    if filename_matches_extension(filename, "tar.gz") {
        return Some("tar.gz");
    }
    if filename_matches_extension(filename, "tgz") {
        return Some("tgz");
    }
    if filename_matches_extension(filename, "zip") {
        return Some("zip");
    }
    if filename_matches_extension(filename, "tar") {
        return Some("tar");
    }
    None
}

fn archive_format_from_configured_filename(filename: &str, config: &ConfigSet) -> Option<String> {
    tar_filters_from_config(config)
        .into_iter()
        .map(|(name, _, _)| name)
        .find(|name| filename_matches_extension(filename, name))
}

fn run_remote_archive(
    url: &str,
    exec: &str,
    p: &ParsedArchive,
    tree_ish: &str,
    _format: &str,
    name_hint: Option<&str>,
) -> Result<()> {
    let resolved_url = resolve_remote_archive_url(url)?;
    let repo_path_raw = if let Some(path) = resolved_url.strip_prefix("file://") {
        PathBuf::from(path)
    } else {
        PathBuf::from(&resolved_url)
    };
    let repo_path = repo_path_raw.canonicalize().unwrap_or(repo_path_raw);
    let exec_trimmed = exec.trim();
    let exec_base = exec_trimmed.rsplit('/').next().unwrap_or(exec_trimmed);
    let is_default_exec = matches!(exec_base, "git-upload-archive" | "upload-archive")
        || exec_trimmed == "git upload-archive";
    let mut child = if is_default_exec {
        let prog = std::env::var("GUST_BIN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("git"));
        let subcmd = exec_base.strip_prefix("git-").unwrap_or(exec_base);
        Command::new(&prog)
            .arg("-C")
            .arg(&repo_path)
            .arg(subcmd)
            .arg(".")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning upload-archive")?
    } else {
        let prog = std::env::var("GUST_BIN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("git"));
        let prog_quoted = shell_single_quote(&prog.to_string_lossy());
        let exec_script = if exec_trimmed.contains("git-upload-archive") {
            exec_trimmed.replace(
                "git-upload-archive",
                &format!("{prog_quoted} upload-archive"),
            )
        } else {
            exec_trimmed.to_owned()
        };
        let repo_arg = repo_path.to_string_lossy().replace('\'', "'\"'\"'");
        let script = format!("{exec_script} '{repo_arg}'");
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .env_remove("GIT_PROTOCOL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd.spawn().context("spawning custom upload-archive")?
    };

    let mut stdin = child.stdin.take().context("upload-archive stdin")?;
    if let Some(hint) = name_hint {
        if let Some(f) = archive_format_from_filename(hint) {
            pkt_line::write_line(&mut stdin, &format!("argument --format={f}"))?;
        }
    }
    for t in &p.tokens {
        match t {
            ArchiveToken::Format(v) => {
                pkt_line::write_line(&mut stdin, &format!("argument --format={v}"))?;
            }
            ArchiveToken::Prefix(v) => {
                pkt_line::write_line(&mut stdin, &format!("argument --prefix={v}"))?;
            }
            ArchiveToken::Mtime(v) => {
                pkt_line::write_line(&mut stdin, &format!("argument --mtime={v}"))?;
            }
            ArchiveToken::Verbose => {
                pkt_line::write_line(&mut stdin, "argument --verbose")?;
            }
            _ => {}
        }
    }
    pkt_line::write_line(&mut stdin, &format!("argument {tree_ish}"))?;
    for ps in &p.pathspecs {
        pkt_line::write_line(&mut stdin, &format!("argument {ps}"))?;
    }
    pkt_line::write_flush(&mut stdin)?;
    drop(stdin);

    let mut stdout = child.stdout.take().context("upload-archive stdout")?;
    let first = pkt_line::read_packet(&mut stdout)?
        .ok_or_else(|| anyhow::anyhow!("upload-archive: unexpected EOF"))?;
    let pkt_line::Packet::Data(line) = first else {
        bail!("upload-archive: expected ACK");
    };
    if !line.starts_with("ACK") {
        bail!("upload-archive: {line}");
    }
    let second = pkt_line::read_packet(&mut stdout)?
        .ok_or_else(|| anyhow::anyhow!("upload-archive: expected flush"))?;
    if !matches!(second, pkt_line::Packet::Flush) {
        bail!("upload-archive: expected flush after ACK");
    }

    let mut raw = Vec::new();
    stdout.read_to_end(&mut raw)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("upload-archive failed with {status}");
    }
    let data = pkt_line::decode_sideband_primary(&raw)?;

    if let Some(path) = name_hint {
        let mut f = File::create(path).with_context(|| format!("creating '{path}'"))?;
        f.write_all(&data)?;
    } else {
        let mut out = io::stdout().lock();
        out.write_all(&data)?;
        out.flush()?;
    }
    Ok(())
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn resolve_remote_archive_url(remote: &str) -> Result<String> {
    if remote.contains("://")
        || remote.starts_with('/')
        || remote.starts_with("./")
        || remote.starts_with("../")
    {
        return Ok(remote.to_owned());
    }
    let repo = Repository::discover(None).context("not a git repository")?;
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let key = format!("remote.{remote}.url");
    if let Some(url) = config.get(&key).filter(|u| !u.trim().is_empty()) {
        if url.contains("://") || url.starts_with('/') {
            return Ok(url);
        }
        let base = repo
            .work_tree
            .clone()
            .unwrap_or_else(|| repo.git_dir.clone());
        return Ok(base.join(url).to_string_lossy().to_string());
    }
    Ok(remote.to_owned())
}

pub fn run(_args: Args) -> Result<()> {
    bail!("internal: use run_from_argv for archive")
}

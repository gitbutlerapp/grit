//! Library-backed workloads matched to common `git` ODB-heavy commands.

use std::fmt::Write as FmtWrite;
use std::io::{self, BufRead, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{write_commits_log_patch, LogPatchCommitHeader, LogPatchOptions};
use grit_lib::error::Error;
use grit_lib::maintenance::{write_commit_graph, CommitGraphOptions};
use grit_lib::objects::pack::{
    read_object_from_pack_at_offset, read_pack_index_cached, PackIndex, SingleLocalPackReader,
};
use grit_lib::objects::{parse_commit, Object, ObjectId};
use grit_lib::remote::serve::{upload_pack, ProtocolVersion, ServeOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::filter::CommitFilterBuilder;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions, RevListResult};
use std::sync::Arc;

/// Parse one stdin line for `git cat-file --batch` / `--batch-check` (hex OID, optional trailing fields).
pub(crate) fn parse_cat_file_batch_oid_line(line: &[u8]) -> Result<ObjectId> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let text = std::str::from_utf8(line).context("oid line utf-8")?;
    ObjectId::from_hex(text).with_context(|| format!("parse oid {text}"))
}

/// Read hex object ids from stdin and emit `git cat-file --batch` lines.
pub fn cat_file_batch(repo: &Repository) -> Result<()> {
    repo.odb().with_pack_read_context(|| {
        let mut stdin = io::stdin().lock();
        let mut stdout = BufWriter::with_capacity(1 << 20, io::stdout().lock());
        let mut line = Vec::with_capacity(64);
        // Fast path: exactly one local pack and no loose objects (see `SingleLocalPackReader::open`).
        // On miss or when disabled, fall back to the full ODB (loose + all packs).
        let mut fast =
            SingleLocalPackReader::open(repo.odb().objects_dir()).context("single-pack reader")?;
        loop {
            line.clear();
            let n = stdin
                .read_until(b'\n', &mut line)
                .context("read oid line")?;
            if n == 0 {
                break;
            }
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let hex = line.strip_suffix(b"\n").unwrap_or(&line);
            let hex_str = std::str::from_utf8(hex).context("oid line utf-8")?;
            let Ok(oid) = parse_cat_file_batch_oid_line(&line) else {
                writeln!(stdout, "{hex_str} missing")?;
                continue;
            };
            if let Some(reader) = fast.as_mut() {
                match reader.read_object(&oid) {
                    Ok(object) => write_batch_object_body(&mut stdout, hex, &object)?,
                    Err(Error::ObjectNotFound(_)) => {
                        write_batch_object(&mut stdout, repo, hex, &oid)?;
                    }
                    Err(e) => return Err(e.into()),
                }
            } else {
                write_batch_object(&mut stdout, repo, hex, &oid)?;
            }
        }
        stdout.flush()?;
        Ok(())
    })
}

/// Read hex object ids from stdin and emit `git cat-file --batch-check` lines.
pub fn cat_file_batch_check(repo: &Repository) -> Result<()> {
    repo.odb().with_pack_read_context(|| {
        let mut stdin = io::stdin().lock();
        let mut stdout = BufWriter::with_capacity(1 << 20, io::stdout().lock());
        let mut line = Vec::with_capacity(64);
        let mut fast =
            SingleLocalPackReader::open(repo.odb().objects_dir()).context("single-pack reader")?;
        loop {
            line.clear();
            let n = stdin
                .read_until(b'\n', &mut line)
                .context("read oid line")?;
            if n == 0 {
                break;
            }
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let hex = line.strip_suffix(b"\n").unwrap_or(&line);
            let hex_str = std::str::from_utf8(hex).context("oid line utf-8")?;
            let Ok(oid) = parse_cat_file_batch_oid_line(&line) else {
                writeln!(stdout, "{hex_str} missing")?;
                continue;
            };
            let info_result = if let Some(reader) = fast.as_mut() {
                match reader.read_info(&oid) {
                    Ok(info) => Ok(info),
                    Err(Error::ObjectNotFound(_)) => repo.odb().read_info(&oid),
                    Err(e) => Err(e),
                }
            } else {
                repo.odb().read_info(&oid)
            };
            match info_result {
                Ok(info) => {
                    writeln!(stdout, "{hex_str} {} {}", info.kind.as_str(), info.size)?;
                }
                Err(Error::ObjectNotFound(_)) => {
                    writeln!(stdout, "{hex_str} missing")?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        stdout.flush()?;
        Ok(())
    })
}

/// Emit every packed object in pack-file offset order (like `cat-file --batch-all-objects --unordered`).
pub fn cat_file_batch_all_unordered(repo: &Repository) -> Result<()> {
    let objects_dir = repo.odb().objects_dir();
    repo.odb().with_pack_read_context(|| {
        let pack_dir = objects_dir.join("pack");
        let mut stdout = BufWriter::with_capacity(1 << 20, io::stdout().lock());
        if !pack_dir.is_dir() {
            return Ok(());
        }
        let mut idx_paths: Vec<_> = std::fs::read_dir(&pack_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "idx"))
            .collect();
        idx_paths.sort();
        for idx_path in idx_paths {
            let idx = read_pack_index_cached(&idx_path)
                .with_context(|| format!("read {}", idx_path.display()))?;
            emit_pack_in_offset_order(&mut stdout, idx.as_ref())?;
        }
        stdout.flush()?;
        Ok(())
    })
}

fn emit_pack_in_offset_order(out: &mut impl Write, idx: &PackIndex) -> Result<()> {
    let offsets: Vec<u64> = (0..idx.len())
        .map(|i| idx.offset_at(i))
        .collect::<std::result::Result<Vec<_>, Error>>()
        .context("pack index offset")?;
    let mut order: Vec<usize> = (0..idx.len()).collect();
    order.sort_by(|&a, &b| offsets[a].cmp(&offsets[b]));
    for i in order {
        let offset = offsets[i];
        let oid = ObjectId::from_bytes(idx.oid_at(i)).context("pack entry oid")?;
        let object = read_object_from_pack_at_offset(idx, offset)
            .with_context(|| format!("read packed object at offset {offset}"))?;
        let hex = oid.to_hex();
        write_batch_object_body(out, hex.as_bytes(), &object)?;
    }
    Ok(())
}

fn write_batch_object(
    out: &mut impl Write,
    repo: &Repository,
    hex: &[u8],
    oid: &ObjectId,
) -> Result<()> {
    let hex_str = std::str::from_utf8(hex).context("oid line utf-8")?;
    match repo.odb().read(oid) {
        Ok(object) => write_batch_object_body(out, hex, &object),
        Err(Error::ObjectNotFound(_)) => {
            writeln!(out, "{hex_str} missing")?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn write_batch_object_body(out: &mut impl Write, hex: &[u8], object: &Object) -> Result<()> {
    let kind = object.kind.as_str();
    let hex = std::str::from_utf8(hex).context("oid line utf-8")?;
    writeln!(out, "{hex} {kind} {}", object.data.len())?;
    out.write_all(&object.data)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Format `git rev-list --objects --all` output from a [`RevListResult`].
///
/// Git lists every selected commit first (in walk order), then reachable tree/blob
/// lines in a single object pass — not interleaved per commit.
pub fn format_rev_list_objects(result: &RevListResult) -> String {
    let mut out = String::new();
    for commit in &result.commits {
        let _ = writeln!(out, "{commit}");
    }
    format_object_lines(&mut out, &result.objects);
    out
}

fn format_object_lines(out: &mut String, objects: &[(ObjectId, String)]) {
    for (oid, path) in objects {
        if path.is_empty() {
            // Git prints a trailing space before the newline for unnamed tree lines.
            let _ = writeln!(out, "{oid} ");
        } else {
            let _ = writeln!(out, "{oid} {path}");
        }
    }
}

/// Walk all refs and print reachable objects (`rev-list --objects --all`).
pub fn rev_list_objects(repo: &Repository) -> Result<()> {
    let opts = RevListOptions {
        all_refs: true,
        objects: true,
        use_commit_graph: true,
        ..Default::default()
    };
    let result = repo
        .odb()
        .with_pack_read_context(|| rev_list(repo, &[], &[], &opts))
        .context("rev-list --objects --all")?;
    let mut stdout = io::stdout().lock();
    for commit in &result.commits {
        writeln!(stdout, "{commit}")?;
    }
    for (oid, path) in &result.objects {
        if path.is_empty() {
            writeln!(stdout, "{oid} ")?;
        } else {
            writeln!(stdout, "{oid} {path}")?;
        }
    }
    Ok(())
}

/// Last `limit` commits with unified diffs (work matched to `git log -p -n`).
pub fn log_patch(repo: &Repository, limit: usize) -> Result<()> {
    repo.odb().with_pack_read_context(|| {
        let opts = RevListOptions {
            max_count: Some(limit),
            use_commit_graph: true,
            ..Default::default()
        };
        let result =
            rev_list(repo, &["HEAD".to_owned()], &[], &opts).context("rev-list for log")?;
        let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
        let mut patch_opts = LogPatchOptions::git_default(&config);
        if std::env::var_os("GRIT_BENCH_LOG_PATCH_PERCENT_H").is_some() {
            patch_opts.commit_header = LogPatchCommitHeader::FormatPercentH;
        }
        let mut stdout = io::BufWriter::with_capacity(1 << 20, io::stdout().lock());
        write_commits_log_patch(repo, &mut stdout, &result.commits, &patch_opts)?;
        stdout.flush()?;
        Ok(())
    })
}

/// `rev-list --count HEAD` (commits reachable from `HEAD`).
fn author_filter(pattern: &str) -> Result<Arc<grit_lib::revwalk::filter::CommitFilter>> {
    Ok(Arc::new(
        CommitFilterBuilder::new()
            .author(pattern)
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    ))
}

/// `rev-list --count --author=<pattern> HEAD`.
pub fn rev_list_count_author_head(repo: &Repository, author: &str) -> Result<()> {
    let opts = RevListOptions {
        count: true,
        commit_filter: Some(author_filter(author)?),
        use_commit_graph: true,
        ..Default::default()
    };
    let result = rev_list(repo, &["HEAD".to_string()], &[], &opts)
        .context("rev-list --count --author HEAD")?;
    println!("{}", result.commit_count());
    Ok(())
}

/// First `--max-count` commits on `HEAD` with `--author`.
pub fn rev_list_author_n_head(repo: &Repository, author: &str, max_count: usize) -> Result<()> {
    let opts = RevListOptions {
        max_count: Some(max_count),
        commit_filter: Some(author_filter(author)?),
        use_commit_graph: true,
        ..Default::default()
    };
    let result =
        rev_list(repo, &["HEAD".to_string()], &[], &opts).context("rev-list --author -n HEAD")?;
    println!("{}", result.commits.len());
    Ok(())
}

pub fn rev_list_count_head(repo: &Repository) -> Result<()> {
    let opts = RevListOptions {
        count: true,
        use_commit_graph: true,
        ..Default::default()
    };
    let result =
        rev_list(repo, &["HEAD".to_string()], &[], &opts).context("rev-list --count HEAD")?;
    let n = result.commit_count();
    println!("{n}");
    Ok(())
}

fn blame_head_start(repo: &Repository) -> Result<grit_lib::objects::ObjectId> {
    use grit_lib::state::{resolve_head, HeadState};
    match resolve_head(repo.git_dir())? {
        HeadState::Detached { oid } => Ok(oid),
        HeadState::Branch { oid: Some(oid), .. } => Ok(oid),
        HeadState::Branch { oid: None, .. } | HeadState::Invalid => {
            anyhow::bail!("cannot blame: HEAD does not resolve to a commit");
        }
    }
}

/// Parse a `START,END` inclusive 1-based line range for blame.
pub fn parse_blame_line_range(spec: &str) -> Result<std::ops::RangeInclusive<usize>> {
    let (start, end) = spec
        .split_once(',')
        .ok_or_else(|| anyhow::anyhow!("expected START,END line range, got {spec:?}"))?;
    let start: usize = start.trim().parse().context("range start")?;
    let end: usize = end.trim().parse().context("range end")?;
    if start == 0 || end < start {
        anyhow::bail!("invalid blame line range {spec}");
    }
    Ok(start..=end)
}

/// Emit `git blame --porcelain` for one worktree path at `HEAD`.
pub fn blame_porcelain(repo: &Repository, path: &str) -> Result<()> {
    blame_porcelain_with_range(repo, path, None)
}

/// Emit porcelain blame for `path` at `HEAD`, optionally limited to a 1-based line range.
pub fn blame_porcelain_with_range(
    repo: &Repository,
    path: &str,
    line_range: Option<std::ops::RangeInclusive<usize>>,
) -> Result<()> {
    use grit_lib::blame::{blame_file, BlameOptions};
    use std::io::{BufWriter, Write};

    let start = blame_head_start(repo)?;
    let result = blame_file(
        repo,
        path,
        &BlameOptions {
            start: Some(start),
            line_range,
            ..Default::default()
        },
    )
    .context("blame_file")?;

    let mut stdout = BufWriter::new(io::stdout().lock());
    let mut i = 0usize;
    while i < result.lines.len() {
        let commit = result.lines[i].commit;
        let orig_start = result.lines[i].original_line;
        let final_start = result.lines[i].final_line;
        let mut count = 1usize;
        while i + count < result.lines.len() && result.lines[i + count].commit == commit {
            count += 1;
        }
        write_blame_porcelain_header(
            &mut stdout,
            repo,
            commit,
            orig_start,
            final_start,
            count,
            path,
            &result.commits,
        )?;
        for line in &result.lines[i..i + count] {
            let content = line.content.trim_end_matches(['\r', '\n']);
            writeln!(stdout, "\t{content}")?;
        }
        i += count;
    }
    stdout.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_blame_porcelain_header(
    out: &mut impl Write,
    repo: &Repository,
    commit: ObjectId,
    orig: usize,
    final_line: usize,
    num_lines: usize,
    path: &str,
    commits: &std::collections::HashMap<ObjectId, grit_lib::blame::BlameCommitInfo>,
) -> Result<()> {
    writeln!(out, "{} {orig} {final_line} {num_lines}", commit.to_hex())?;
    let mut block = String::new();
    let meta = commits.get(&commit);
    let obj = repo.odb().read(&commit).ok();
    let parsed = obj.as_ref().and_then(|o| parse_commit(&o.data).ok());
    let author = meta
        .map(|m| m.author.as_str())
        .or_else(|| parsed.as_ref().map(|c| c.author.as_str()))
        .unwrap_or("");
    let committer = parsed
        .as_ref()
        .map(|c| c.committer.as_str())
        .unwrap_or(author);
    let summary = meta
        .map(|m| m.summary.as_str())
        .or_else(|| {
            parsed.as_ref().and_then(|c| {
                c.message
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map(str::trim)
            })
        })
        .unwrap_or("");
    let (author_name, author_mail, author_time, author_tz) = split_identity(author);
    let (committer_name, committer_mail, committer_time, committer_tz) = split_identity(committer);
    block.push_str(&format!("author {author_name}\n"));
    block.push_str(&format!("author-mail {author_mail}\n"));
    block.push_str(&format!("author-time {author_time}\n"));
    block.push_str(&format!("author-tz {author_tz}\n"));
    block.push_str(&format!("committer {committer_name}\n"));
    block.push_str(&format!("committer-mail {committer_mail}\n"));
    block.push_str(&format!("committer-time {committer_time}\n"));
    block.push_str(&format!("committer-tz {committer_tz}\n"));
    block.push_str(&format!("summary {summary}\n"));
    block.push_str(&format!("filename {path}\n"));
    write!(out, "{block}")?;
    Ok(())
}

fn split_identity(identity: &str) -> (String, String, String, String) {
    let parts: Vec<&str> = identity.rsplitn(3, ' ').collect();
    if parts.len() >= 3 {
        let tz = parts[0].to_string();
        let time = parts[1].to_string();
        let name_mail = parts[2..].join(" ");
        let mail = name_mail
            .split_once('<')
            .and_then(|(_, rest)| rest.strip_suffix('>'))
            .map(|m| format!("<{m}>"))
            .unwrap_or_else(|| "<>".to_string());
        let name = name_mail
            .split_once('<')
            .map(|(n, _)| n.trim())
            .unwrap_or("");
        return (name.to_string(), mail, time, tz);
    }
    (
        identity.to_string(),
        "<>".to_string(),
        "0".to_string(),
        "+0000".to_string(),
    )
}

/// `rev-list --count --all` (commit count only).
pub fn rev_list_count(repo: &Repository) -> Result<()> {
    let opts = RevListOptions {
        all_refs: true,
        count: true,
        use_commit_graph: true,
        ..Default::default()
    };
    let result = rev_list(repo, &[], &[], &opts).context("rev-list --count --all")?;
    let n = result.commit_count();
    println!("{n}");
    Ok(())
}

/// `rev-list --count --objects --all --use-bitmap-index`.
pub fn rev_list_count_objects(repo: &Repository) -> Result<()> {
    let opts = RevListOptions {
        all_refs: true,
        count: true,
        objects: true,
        use_bitmap_index: true,
        use_commit_graph: true,
        ..Default::default()
    };
    let result = repo
        .odb()
        .with_pack_read_context(|| rev_list(repo, &[], &[], &opts))
        .context("rev-list --count --objects --all")?;
    let n = result.count_with_objects_total();
    println!("{n}");
    Ok(())
}

struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stateless v0 upload-pack serving every ref; output goes to a sink, bytes on stdout.
pub fn serve_clone(repo: &Repository) -> Result<()> {
    let req_path = repo
        .git_dir()
        .join(crate::serve_request::SERVE_CLONE_REQUEST_FILE);
    let req = std::fs::read(&req_path)
        .with_context(|| format!("read serve-clone request {}", req_path.display()))?;
    let mut input: &[u8] = &req;
    let mut sink = CountingWriter { bytes: 0 };
    let opts = ServeOptions {
        protocol: ProtocolVersion::V0,
        stateless_rpc: true,
        advertise_refs: false,
        agent: String::new(),
        hidden_refs: Vec::new(),
    };
    upload_pack(repo, &mut input, &mut sink, &opts).context("upload-pack serve-clone")?;
    println!("{}", sink.bytes);
    Ok(())
}

/// Materialize `branch` in the worktree (matches `git checkout -f` after `--no-checkout`).
pub fn checkout_branch_force(repo: &Repository, branch: &str) -> Result<()> {
    use grit_lib::objects::parse_commit;
    use grit_lib::porcelain::checkout::checkout_between_trees;
    let oid = if branch.eq_ignore_ascii_case("HEAD") {
        *grit_lib::state::resolve_head(repo.git_dir())?
            .oid()
            .context("HEAD does not resolve to a commit")?
    } else {
        repo.references()
            .resolve(
                &grit_lib::refs::store::RefName::try_from(branch)
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            )
            .with_context(|| format!("resolve branch {branch}"))?
    };
    let commit = parse_commit(
        &repo
            .odb()
            .read(&oid)
            .with_context(|| format!("read commit {oid}"))?
            .data,
    )
    .context("parse commit")?;
    checkout_between_trees(repo, None, &commit.tree).context("checkout branch")?;
    Ok(())
}

/// Build in-memory pack index records for a `.pack` file (index-pack CPU path).
pub fn index_pack_records(
    pack_path: &Path,
    repo: &Repository,
    threads: Option<usize>,
) -> Result<()> {
    use grit_lib::objects::hash::Parallelism;
    use grit_lib::objects::unpack_objects::pack_index_records_with_threads;

    let pack =
        std::fs::read(pack_path).with_context(|| format!("read pack {}", pack_path.display()))?;
    let parallelism = match threads {
        Some(n) => Parallelism::resolve(Some(n)),
        None => repo
            .config()
            .map(|cfg| cfg.pack_index_parallelism())
            .unwrap_or_else(|_| Parallelism::resolve(None)),
    };
    let _records =
        pack_index_records_with_threads(&pack, repo.odb(), parallelism).context("index-pack")?;
    Ok(())
}

/// Write a full `--all` pack to stdout (matches `git pack-objects --all --stdout` workload).
pub fn pack_build_all_stdout(repo: &Repository, delta_threads: usize) -> Result<()> {
    use grit_lib::objects::hash::Parallelism;
    use grit_lib::objects::pack_objects::{PackBuildOptions, PackObjects, PackObjectsOptions};
    use grit_lib::refs::list_refs_for_repository;

    let refs = list_refs_for_repository(repo, "")?;
    let wants: Vec<_> = refs.into_iter().map(|(_, oid)| oid).collect();
    let workers = if delta_threads == 0 {
        Parallelism::resolve(None).threads().get()
    } else {
        delta_threads.max(1)
    };
    let opts = PackObjectsOptions {
        build: PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
        search_threads: None,
        write_threads: Some(workers),
    };
    let mut stdout = io::stdout().lock();
    PackObjects::new(repo.odb(), opts)
        .wants(&wants)
        .write_to(&mut stdout)
        .context("pack-objects --all")?;
    Ok(())
}

/// Tree diff with rename detection (`git diff -M` between two commits).
pub fn diff_rename_m(
    repo: &Repository,
    old_rev: &str,
    new_rev: &str,
    threshold: u32,
) -> Result<()> {
    use grit_lib::diff::{detect_renames, diff_trees};
    use grit_lib::objects::parse_commit;

    let old_oid = resolve_rev(repo, old_rev)?;
    let new_oid = resolve_rev(repo, new_rev)?;
    let old_raw = repo.odb().read(&old_oid).context("read old commit")?;
    let new_raw = repo.odb().read(&new_oid).context("read new commit")?;
    let old_commit = parse_commit(&old_raw.data).context("parse old commit")?;
    let new_commit = parse_commit(&new_raw.data).context("parse new commit")?;
    let entries = diff_trees(
        repo.odb(),
        Some(&old_commit.tree),
        Some(&new_commit.tree),
        "",
    )?;
    let entries = detect_renames(repo.odb(), None, entries, threshold);
    let mut out = io::stdout().lock();
    for entry in entries {
        let path = entry
            .new_path
            .as_deref()
            .or(entry.old_path.as_deref())
            .unwrap_or("");
        writeln!(out, "{path}")?;
    }
    Ok(())
}

fn resolve_rev(repo: &Repository, rev: &str) -> Result<ObjectId> {
    if rev.len() >= 4 && rev.chars().all(|c| c.is_ascii_hexdigit()) {
        return ObjectId::from_hex(rev).context("parse oid");
    }
    let mut cmd = std::process::Command::new("git");
    let cwd = repo.work_tree().unwrap_or_else(|| repo.git_dir());
    cmd.args(["rev-parse", rev]).current_dir(cwd);
    let out = cmd.output().context("git rev-parse in driver")?;
    if !out.status.success() {
        anyhow::bail!(
            "rev-parse {rev}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).context("parse rev-parse oid")
}

/// Write a reachable commit-graph with changed-path Bloom filters (maintenance API).
pub fn commit_graph_write(repo: &Repository) -> Result<()> {
    write_commit_graph(repo, &CommitGraphOptions::with_changed_paths(true))?;
    Ok(())
}

/// Open a bare or normal repo at `path` for driver subcommands.
pub fn open_repo(path: &Path) -> Result<Repository> {
    if path.join("HEAD").is_file() {
        Ok(Repository::open(path, &RepositoryOptions::empty())?)
    } else {
        Ok(Repository::discover(
            Some(path),
            &RepositoryOptions::empty(),
        )?)
    }
}

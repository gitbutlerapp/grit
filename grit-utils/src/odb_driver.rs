//! Library-backed workloads matched to common `git` ODB-heavy commands.

use std::fmt::Write as FmtWrite;
use std::io::{self, BufRead, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use grit_lib::diff::{diff_trees, unified_diff_with_prefix, DiffStatus};
use grit_lib::error::Error;
use grit_lib::objects::{parse_commit, Object, ObjectId, ObjectKind};
use grit_lib::pack::{read_object_from_pack_at_offset, read_pack_index_cached, PackIndex};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions, RevListResult};
use grit_lib::serve::{upload_pack, ProtocolVersion, ServeOptions};

/// Read hex object ids from stdin and emit `git cat-file --batch` lines.
pub fn cat_file_batch(repo: &Repository) -> Result<()> {
    repo.odb.with_pack_read_context(|| {
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line.context("read oid line")?;
            let hex = line.trim();
            if hex.is_empty() {
                continue;
            }
            let oid = ObjectId::from_hex(hex).with_context(|| format!("parse oid {hex}"))?;
            write_batch_object(&mut stdout, repo, &oid)?;
        }
        Ok(())
    })
}

/// Read hex object ids from stdin and emit `git cat-file --batch-check` lines.
pub fn cat_file_batch_check(repo: &Repository) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("read oid line")?;
        let hex = line.trim();
        if hex.is_empty() {
            continue;
        }
        let oid = ObjectId::from_hex(hex).with_context(|| format!("parse oid {hex}"))?;
        match repo.odb.read_info(&oid) {
            Ok(info) => {
                writeln!(stdout, "{oid} {} {}", info.kind.as_str(), info.size)?;
            }
            Err(Error::ObjectNotFound(_)) => {
                writeln!(stdout, "{oid} missing")?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Emit every packed object in pack-file offset order (like `cat-file --batch-all-objects --unordered`).
pub fn cat_file_batch_all_unordered(repo: &Repository) -> Result<()> {
    let objects_dir = repo.odb.objects_dir();
    repo.odb.with_pack_read_context(|| {
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
    let mut order: Vec<usize> = (0..idx.len()).collect();
    order.sort_by_key(|&i| idx.offset_at(i));
    for i in order {
        let offset = idx.offset_at(i);
        let oid = ObjectId::from_bytes(idx.oid_at(i)).context("pack entry oid")?;
        let object = read_object_from_pack_at_offset(idx, offset)
            .with_context(|| format!("read packed object at offset {offset}"))?;
        write_batch_object_body(out, &oid, &object)?;
    }
    Ok(())
}

fn write_batch_object(out: &mut impl Write, repo: &Repository, oid: &ObjectId) -> Result<()> {
    let object = repo.odb.read(oid).with_context(|| format!("read {oid}"))?;
    write_batch_object_body(out, oid, &object)
}

fn write_batch_object_body(out: &mut impl Write, oid: &ObjectId, object: &Object) -> Result<()> {
    let kind = object.kind.as_str();
    writeln!(out, "{oid} {kind} {}", object.data.len())?;
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
        .odb
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
    let opts = RevListOptions {
        max_count: Some(limit),
        ..Default::default()
    };
    let result = rev_list(repo, &["HEAD".to_owned()], &[], &opts).context("rev-list for log")?;
    let mut stdout = io::stdout().lock();
    let commits = &result.commits;
    for (i, oid) in commits.iter().enumerate() {
        write_commit_patch(&mut stdout, repo, oid)?;
        if i + 1 < commits.len() {
            writeln!(stdout)?;
        }
    }
    Ok(())
}

fn write_commit_patch(out: &mut impl Write, repo: &Repository, oid: &ObjectId) -> Result<()> {
    let raw = repo.odb.read(oid).context("read commit")?;
    let commit = parse_commit(&raw.data).context("parse commit")?;
    writeln!(out, "commit {oid}")?;
    if commit.parents.len() > 1 {
        let mut parts = Vec::with_capacity(commit.parents.len());
        for parent in &commit.parents {
            parts.push(abbreviate_oid(repo, parent)?);
        }
        writeln!(out, "Merge: {}", parts.join(" "))?;
    }
    writeln!(out, "Author: {}", format_author_display(&commit.author))?;
    writeln!(out, "Date:   {}", format_author_date(&commit.author))?;
    writeln!(out)?;
    for line in commit.message.lines() {
        writeln!(out, "    {line}")?;
    }
    // Default `git log -p` does not emit patches for merge commits.
    if commit.parents.len() > 1 {
        return Ok(());
    }
    writeln!(out)?;
    let parent_tree = if commit.parents.is_empty() {
        None
    } else {
        let parent_oid = commit.parents[0];
        let parent_raw = repo.odb.read(&parent_oid).context("read parent")?;
        Some(parse_commit(&parent_raw.data).context("parse parent")?.tree)
    };
    let entries = diff_trees(&repo.odb, parent_tree.as_ref(), Some(&commit.tree), "")?;
    for entry in entries {
        write_diff_entry(out, repo, &entry)?;
    }
    Ok(())
}

fn write_diff_entry(
    out: &mut impl Write,
    repo: &Repository,
    entry: &grit_lib::diff::DiffEntry,
) -> Result<()> {
    let old_path = entry.old_path.as_deref().unwrap_or("/dev/null");
    let new_path = entry.new_path.as_deref().unwrap_or("/dev/null");
    let header_path = entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .unwrap_or("");
    writeln!(out, "diff --git a/{header_path} b/{header_path}")?;
    match entry.status {
        DiffStatus::Added => {
            writeln!(out, "new file mode {}", entry.new_mode)?;
            writeln!(
                out,
                "index {}..{}",
                abbreviate_oid(repo, &grit_lib::diff::zero_oid())?,
                abbreviate_oid(repo, &entry.new_oid)?
            )?;
        }
        DiffStatus::Deleted => {
            writeln!(out, "deleted file mode {}", entry.old_mode)?;
            writeln!(
                out,
                "index {}..{}",
                abbreviate_oid(repo, &entry.old_oid)?,
                abbreviate_oid(repo, &grit_lib::diff::zero_oid())?
            )?;
        }
        DiffStatus::Modified => {
            writeln!(
                out,
                "index {}..{} {}",
                abbreviate_oid(repo, &entry.old_oid)?,
                abbreviate_oid(repo, &entry.new_oid)?,
                entry.new_mode
            )?;
        }
        DiffStatus::Renamed | DiffStatus::Copied => {
            writeln!(out, "similarity index {}%", entry.score.unwrap_or(100))?;
            writeln!(out, "rename from {old_path}")?;
            writeln!(out, "rename to {new_path}")?;
            writeln!(
                out,
                "index {}..{}",
                abbreviate_oid(repo, &entry.old_oid)?,
                abbreviate_oid(repo, &entry.new_oid)?
            )?;
        }
        _ => {
            writeln!(
                out,
                "index {}..{}",
                abbreviate_oid(repo, &entry.old_oid)?,
                abbreviate_oid(repo, &entry.new_oid)?
            )?;
        }
    }
    let old_bytes = read_blob_or_empty(repo, entry.old_oid, entry.status == DiffStatus::Added)?;
    let new_bytes = read_blob_or_empty(repo, entry.new_oid, entry.status == DiffStatus::Deleted)?;
    if old_bytes.contains(&0) || new_bytes.contains(&0) {
        let old_disp = log_path_for_binary(old_path, "a/");
        let new_disp = log_path_for_binary(new_path, "b/");
        writeln!(out, "Binary files {old_disp} and {new_disp} differ")?;
        return Ok(());
    }
    let old_text = String::from_utf8_lossy(&old_bytes);
    let new_text = String::from_utf8_lossy(&new_bytes);
    let patch = unified_diff_with_prefix(
        &old_text, &new_text, old_path, new_path, 3, 0, "a/", "b/", true, false,
    );
    write!(out, "{patch}")?;
    if !patch.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

fn log_path_for_binary(path: &str, prefix: &str) -> String {
    if path == "/dev/null" {
        path.to_owned()
    } else {
        format!("{prefix}{path}")
    }
}

fn min_abbrev_len(repo: &Repository) -> usize {
    repo.config()
        .ok()
        .and_then(|cfg| cfg.get("core.abbrev"))
        .and_then(|v| v.parse::<i64>().ok())
        .map(|n| n as usize)
        .unwrap_or(7)
        .clamp(4, 40)
}

fn collect_hex_ids_for_abbrev(repo: &Repository) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let objects = repo.git_dir.join("objects");
    collect_loose_hex_ids(&objects, &mut ids)?;
    let pack_dir = objects.join("pack");
    if pack_dir.is_dir() {
        for entry in std::fs::read_dir(&pack_dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "idx") {
                let idx = read_pack_index_cached(&path)?;
                for ent in idx.iter() {
                    ids.push(
                        ObjectId::from_bytes(ent.oid())
                            .context("pack oid")?
                            .to_hex(),
                    );
                }
            }
        }
    }
    Ok(ids)
}

fn collect_loose_hex_ids(objects: &Path, ids: &mut Vec<String>) -> Result<()> {
    let read = match std::fs::read_dir(objects) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for dir_entry in read {
        let dir_entry = dir_entry?;
        let name = dir_entry.file_name();
        let Some(prefix) = name.to_str() else {
            continue;
        };
        if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if !dir_entry.file_type()?.is_dir() {
            continue;
        }
        for file_entry in std::fs::read_dir(dir_entry.path())? {
            let file_entry = file_entry?;
            if !file_entry.file_type()?.is_file() {
                continue;
            }
            let suffix_os = file_entry.file_name();
            let Some(suffix) = suffix_os.to_str() else {
                continue;
            };
            if suffix.len() >= 38 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
                ids.push(format!("{prefix}{suffix}"));
            }
        }
    }
    Ok(())
}

fn abbreviate_oid(repo: &Repository, oid: &ObjectId) -> Result<String> {
    let min_len = min_abbrev_len(repo);
    let target = oid.to_hex();
    if !repo.odb.exists(oid) {
        return Ok(target[..min_len.min(target.len())].to_owned());
    }
    let all = collect_hex_ids_for_abbrev(repo)?;
    for len in min_len..=40 {
        let prefix = &target[..len];
        let matches = all.iter().filter(|c| c.starts_with(prefix)).count();
        if matches <= 1 {
            return Ok(prefix.to_owned());
        }
    }
    Ok(target)
}

fn format_author_display(ident: &str) -> String {
    let name = ident.find('<').map_or(ident.trim(), |i| ident[..i].trim());
    let email = ident
        .find('<')
        .and_then(|start| ident.find('>').map(|end| &ident[start..=end]))
        .unwrap_or("");
    format!("{name} {email}")
}

fn format_author_date(ident: &str) -> String {
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() < 2 {
        return String::new();
    }
    let ts_str = parts[1];
    let offset_str = parts[0];
    let Ok(ts) = ts_str.parse::<i64>() else {
        return format!("{ts_str} {offset_str}");
    };
    let tz_bytes = offset_str.as_bytes();
    let tz_secs: i64 = if tz_bytes.len() >= 5 {
        let sign = if tz_bytes[0] == b'-' { -1 } else { 1 };
        let h: i64 = offset_str[1..3].parse().unwrap_or(0);
        let m: i64 = offset_str[3..5].parse().unwrap_or(0);
        sign * (h * 3600 + m * 60)
    } else {
        0
    };
    let adjusted = ts + tz_secs;
    let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let weekday = match dt.weekday() {
        time::Weekday::Monday => "Mon",
        time::Weekday::Tuesday => "Tue",
        time::Weekday::Wednesday => "Wed",
        time::Weekday::Thursday => "Thu",
        time::Weekday::Friday => "Fri",
        time::Weekday::Saturday => "Sat",
        time::Weekday::Sunday => "Sun",
    };
    let month = match dt.month() {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    };
    format!(
        "{weekday} {month} {} {:02}:{:02}:{:02} {} {offset_str}",
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.year()
    )
}

fn read_blob_or_empty(repo: &Repository, oid: ObjectId, missing: bool) -> Result<Vec<u8>> {
    if missing {
        return Ok(Vec::new());
    }
    let object = repo.odb.read(&oid)?;
    if object.kind != ObjectKind::Blob {
        return Ok(Vec::new());
    }
    Ok(object.data)
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
    let n = result.commits.len();
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
        .odb
        .with_pack_read_context(|| rev_list(repo, &[], &[], &opts))
        .context("rev-list --count --objects --all")?;
    let n = result.commits.len() + result.objects.len();
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
        .git_dir
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

/// Open a bare or normal repo at `path` for driver subcommands.
pub fn open_repo(path: &Path) -> Result<Repository> {
    if path.join("HEAD").is_file() {
        Ok(Repository::open(path, None)?)
    } else {
        Ok(Repository::discover(Some(path))?)
    }
}

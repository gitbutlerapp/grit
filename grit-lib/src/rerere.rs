//! Git-compatible rerere (`MERGE_RR`, `rr-cache/`, conflict ID hashing).
//!
//! Behaviour matches `git/rerere.c` for upstream harness tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ConfigSet;
use crate::error::Result;
use crate::hash::ObjectHasher;
use crate::index::{entry_from_metadata, Index, IndexEntry, MODE_REGULAR};
use crate::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::repo::Repository;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RerereAutoupdate {
    #[default]
    FromConfig,
    Yes,
    No,
}

/// Outcome of rerere handling one path (replaces stderr status lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerereEventKind {
    RecordedPreimage,
    RecordedResolution,
    ResolvedFromPrevious,
    StagedFromPrevious,
    UpdatedPreimage,
    Forgot,
    NoRemembered,
}

/// One rerere status event for a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerereEvent {
    pub path: String,
    pub kind: RerereEventKind,
}

#[derive(Debug, Clone)]
struct RerereId {
    hex: String,
    variant: i32,
}

#[derive(Debug, Default, Clone)]
struct MergeRrEntry {
    id: Option<RerereId>,
}

fn merge_rr_path(git_dir: &Path) -> PathBuf {
    git_dir.join("MERGE_RR")
}

fn rr_root(git_dir: &Path) -> PathBuf {
    git_dir.join("rr-cache")
}

pub fn rerere_enabled(config: &ConfigSet, git_dir: &Path) -> bool {
    if let Some(val) = config.get("rerere.enabled") {
        let v = val.to_ascii_lowercase();
        if matches!(v.as_str(), "false" | "0" | "no" | "off") {
            return false;
        }
        if matches!(v.as_str(), "true" | "1" | "yes" | "on") {
            return true;
        }
    }
    rr_root(git_dir).is_dir()
}

fn autoupdate_flag(config: &ConfigSet, o: RerereAutoupdate) -> bool {
    match o {
        RerereAutoupdate::Yes => true,
        RerereAutoupdate::No => false,
        RerereAutoupdate::FromConfig => config
            .get("rerere.autoupdate")
            .map(|v| {
                let l = v.to_ascii_lowercase();
                matches!(l.as_str(), "true" | "1" | "yes" | "on")
            })
            .unwrap_or(false),
    }
}

fn read_merge_rr(git_dir: &Path) -> Result<BTreeMap<String, MergeRrEntry>> {
    let path = merge_rr_path(git_dir);
    let mut out = BTreeMap::new();
    let Ok(data) = fs::read(&path) else {
        return Ok(out);
    };
    let mut i = 0usize;
    while i < data.len() {
        let rest = &data[i..];
        let Some(nul) = rest.iter().position(|&b| b == 0) else {
            break;
        };
        let record = std::str::from_utf8(&rest[..nul]).unwrap_or("");
        i += nul + 1;
        let Some(tab) = record.find('\t') else {
            continue;
        };
        let id_part = &record[..tab];
        let path_str = record[tab + 1..].to_string();
        let (hex, variant) = if let Some(dot) = id_part.find('.') {
            let v: i32 = id_part[dot + 1..].parse().unwrap_or(0);
            (id_part[..dot].to_string(), v)
        } else {
            (id_part.to_string(), 0)
        };
        out.insert(
            path_str,
            MergeRrEntry {
                id: Some(RerereId { hex, variant }),
            },
        );
    }
    Ok(out)
}

fn write_merge_rr(git_dir: &Path, entries: &BTreeMap<String, MergeRrEntry>) -> Result<()> {
    let path = merge_rr_path(git_dir);
    if entries.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    let mut buf: Vec<u8> = Vec::new();
    for (path_str, ent) in entries {
        let Some(id) = &ent.id else {
            continue;
        };
        if id.variant > 0 {
            buf.extend_from_slice(format!("{}.{}\t{}\0", id.hex, id.variant, path_str).as_bytes());
        } else {
            buf.extend_from_slice(format!("{}\t{}\0", id.hex, path_str).as_bytes());
        }
    }
    fs::write(&path, buf)?;
    Ok(())
}

fn rr_hex_dir(git_dir: &Path, hex: &str) -> PathBuf {
    rr_root(git_dir).join(hex)
}

fn preimage_path(git_dir: &Path, id: &RerereId) -> PathBuf {
    let b = rr_hex_dir(git_dir, &id.hex);
    if id.variant > 0 {
        b.join(format!("preimage.{}", id.variant))
    } else {
        b.join("preimage")
    }
}

fn postimage_path(git_dir: &Path, id: &RerereId) -> PathBuf {
    let b = rr_hex_dir(git_dir, &id.hex);
    if id.variant > 0 {
        b.join(format!("postimage.{}", id.variant))
    } else {
        b.join("postimage")
    }
}

fn is_cmarker(line: &str, marker_char: u8, marker_size: usize) -> bool {
    let b = line.as_bytes();
    if b.len() < marker_size {
        return false;
    }
    if !b[..marker_size].iter().all(|&byte| byte == marker_char) {
        return false;
    }
    let want_sp = marker_char == b'<' || marker_char == b'>';
    if want_sp {
        if b.get(marker_size).copied() != Some(b' ') {
            return false;
        }
    } else if marker_size < b.len() {
        let c = b[marker_size];
        if !c.is_ascii_whitespace() {
            return false;
        }
    }
    true
}

fn put_marker(out: &mut String, ch: char, size: usize) {
    for _ in 0..size {
        out.push(ch);
    }
    out.push('\n');
}

/// Parse one conflict hunk. `i` is advanced past the closing `>>>>>>>`.
/// First line at entry must be the line *after* the opening `<<<<<<<`.
fn handle_conflict(
    lines: &[String],
    i: &mut usize,
    marker_size: usize,
    ctx: Option<&mut ObjectHasher>,
) -> std::result::Result<String, ()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Hunk {
        Side1,
        Side2,
        Orig,
    }

    let mut hunk = Hunk::Side1;
    let mut one = String::new();
    let mut two = String::new();

    while *i < lines.len() {
        let buf = &lines[*i];
        if is_cmarker(buf, b'<', marker_size) {
            *i += 1;
            let nested = handle_conflict(lines, i, marker_size, None)?;
            match hunk {
                Hunk::Side1 => one.push_str(&nested),
                Hunk::Orig => {}
                Hunk::Side2 => two.push_str(&nested),
            }
        } else if is_cmarker(buf, b'|', marker_size) {
            *i += 1;
            if hunk != Hunk::Side1 {
                return Err(());
            }
            hunk = Hunk::Orig;
        } else if is_cmarker(buf, b'=', marker_size) {
            *i += 1;
            if hunk != Hunk::Side1 && hunk != Hunk::Orig {
                return Err(());
            }
            hunk = Hunk::Side2;
        } else if is_cmarker(buf, b'>', marker_size) {
            *i += 1;
            if hunk != Hunk::Side2 {
                return Err(());
            }
            if one > two {
                std::mem::swap(&mut one, &mut two);
            }
            let mut out = String::new();
            put_marker(&mut out, '<', marker_size);
            out.push_str(&one);
            put_marker(&mut out, '=', marker_size);
            out.push_str(&two);
            put_marker(&mut out, '>', marker_size);
            if let Some(h) = ctx {
                h.update(one.as_bytes());
                h.update(&[0]);
                h.update(two.as_bytes());
                h.update(&[0]);
            }
            return Ok(out);
        } else {
            *i += 1;
            if hunk == Hunk::Side1 {
                one.push_str(buf);
                one.push('\n');
            } else if hunk == Hunk::Orig {
            } else {
                two.push_str(buf);
                two.push('\n');
            }
        }
    }
    Err(())
}

/// Returns 1 if conflict hunks found, 0 if none, -1 on parse error.
/// Full file with conflict markers normalized (Git rerere preimage shape).
fn normalize_conflicts(content: &str, marker_size: usize) -> std::result::Result<String, ()> {
    let lines: Vec<String> = content.lines().map(String::from).collect();
    let mut i = 0usize;
    let mut out = String::new();
    while i < lines.len() {
        let line = &lines[i];
        if is_cmarker(line, b'<', marker_size) {
            i += 1;
            out.push_str(&handle_conflict(&lines, &mut i, marker_size, None)?);
        } else {
            out.push_str(line);
            out.push('\n');
            i += 1;
        }
    }
    Ok(out)
}

fn handle_path(
    content: &str,
    marker_size: usize,
    algo: HashAlgo,
    hash_out: Option<&mut [u8]>,
) -> i32 {
    let lines: Vec<String> = content.lines().map(String::from).collect();
    let mut i = 0usize;
    let mut ctx = if hash_out.is_some() {
        Some(ObjectHasher::new(algo))
    } else {
        None
    };
    let mut out = String::new();
    let mut found = 0i32;

    while i < lines.len() {
        let line = &lines[i];
        if is_cmarker(line, b'<', marker_size) {
            i += 1;
            match handle_conflict(&lines, &mut i, marker_size, ctx.as_mut()) {
                Ok(norm) => {
                    found = 1;
                    out.push_str(&norm);
                }
                Err(()) => return -1,
            }
        } else {
            out.push_str(line);
            out.push('\n');
            i += 1;
        }
    }

    if let (Some(h), Some(buf)) = (ctx, hash_out) {
        let _ = h.finalize_into(buf);
    }
    if found == 1 {
        1
    } else {
        0
    }
}

fn conflict_marker_size(_path: &str) -> usize {
    7
}

/// Compute the rerere conflict identifier for `content` using `algo`.
///
/// Returns `None` when the text does not contain parseable conflict markers.
#[must_use]
pub fn rerere_conflict_id(algo: HashAlgo, content: &str, marker_size: usize) -> Option<ObjectId> {
    let digest_len = algo.len();
    let mut hash = [0u8; 32];
    if handle_path(content, marker_size, algo, Some(&mut hash[..digest_len])) != 1 {
        return None;
    }
    ObjectId::from_bytes(&hash[..digest_len]).ok()
}

fn check_one_conflict(index: &Index, start: usize) -> (usize, u8) {
    let e = &index.entries[start];
    if e.stage() == 0 {
        return (start + 1, 0);
    }

    let mut i = start;
    while i < index.entries.len()
        && index.entries[i].path == e.path
        && index.entries[i].stage() == 1
    {
        i += 1;
    }

    let mut ty = 1u8;
    if i + 1 < index.entries.len() {
        let e2 = &index.entries[i];
        let e3 = &index.entries[i + 1];
        if e2.path == e.path
            && e3.path == e.path
            && e2.stage() == 2
            && e3.stage() == 3
            && matches!(e2.mode, MODE_REGULAR | 0o100755)
            && matches!(e3.mode, MODE_REGULAR | 0o100755)
        {
            ty = 2;
        }
    }

    while i < index.entries.len() && index.entries[i].path == e.path {
        i += 1;
    }
    (i, ty)
}

fn find_three_way_conflicts(index: &Index) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < index.entries.len() {
        let (next, ty) = check_one_conflict(index, i);
        if ty == 2 {
            let path = String::from_utf8_lossy(&index.entries[i].path).to_string();
            out.push(path);
        }
        i = next;
    }
    out
}

/// Add/add (and similar): unmerged path with stages 2 and 3 but no stage 1.
#[allow(dead_code)]
fn find_two_way_unmerged(index: &Index) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < index.entries.len() {
        let e = &index.entries[i];
        if e.stage() == 0 {
            i += 1;
            continue;
        }
        let path = e.path.clone();
        let mut has1 = false;
        let mut has2 = false;
        let mut has3 = false;
        let mut j = i;
        while j < index.entries.len() && index.entries[j].path == path {
            match index.entries[j].stage() {
                1 => has1 = true,
                2 => has2 = true,
                3 => has3 = true,
                _ => {}
            }
            j += 1;
        }
        if has2 && has3 && !has1 {
            out.push(String::from_utf8_lossy(&path).to_string());
        }
        i = j;
    }
    out
}

#[allow(dead_code)]
fn all_rerere_conflict_paths(index: &Index) -> Vec<String> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for p in find_three_way_conflicts(index) {
        paths.insert(p);
    }
    for p in find_two_way_unmerged(index) {
        paths.insert(p);
    }
    paths.into_iter().collect()
}

fn read_blob(odb: &crate::odb::Odb, oid: ObjectId) -> Result<Vec<u8>> {
    let obj = odb.read(&oid)?;
    Ok(obj.data)
}

fn synthesize_conflict_from_index(
    repo: &Repository,
    index: &Index,
    path: &str,
) -> Result<Option<String>> {
    let path_b = path.as_bytes();
    let mut stages: [Option<IndexEntry>; 3] = [None, None, None];
    for e in &index.entries {
        if e.path == path_b {
            let s = e.stage();
            if (1..=3).contains(&s) {
                stages[s as usize - 1] = Some(e.clone());
            }
        }
    }
    if stages.iter().all(Option::is_none) {
        if let Some(record) = index
            .resolve_undo
            .as_ref()
            .and_then(|records| records.get(path_b))
        {
            for stage in 1..=3 {
                let idx = stage - 1;
                if record.modes[idx] == 0 {
                    continue;
                }
                stages[idx] = Some(IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: record.modes[idx],
                    uid: 0,
                    gid: 0,
                    size: 0,
                    oid: record.oids[idx],
                    flags: ((stage as u16) << 12) | (path_b.len().min(0x0fff) as u16),
                    flags_extended: None,
                    path: path_b.to_vec(),
                    base_index_pos: 0,
                });
            }
        }
    }
    let marker_size = conflict_marker_size(path);
    let out = match (&stages[0], &stages[1], &stages[2]) {
        (Some(e1), Some(e2), Some(e3)) => {
            let base = read_blob(&repo.odb, e1.oid)?;
            let ours = read_blob(&repo.odb, e2.oid)?;
            let theirs = read_blob(&repo.odb, e3.oid)?;
            merge(&MergeInput {
                base: &base,
                ours: &ours,
                theirs: &theirs,
                label_ours: "ours",
                label_base: "base",
                label_theirs: "theirs",
                favor: MergeFavor::None,
                style: ConflictStyle::Merge,
                marker_size,
                diff_algorithm: None,
                ignore_all_space: false,
                ignore_space_change: false,
                ignore_space_at_eol: false,
                ignore_cr_at_eol: false,
            })?
        }
        (None, Some(e2), Some(e3)) => {
            let ours = read_blob(&repo.odb, e2.oid)?;
            let theirs = read_blob(&repo.odb, e3.oid)?;
            merge(&MergeInput {
                base: &[],
                ours: &ours,
                theirs: &theirs,
                label_ours: "HEAD",
                label_base: "empty tree",
                label_theirs: path,
                favor: MergeFavor::None,
                style: ConflictStyle::Merge,
                marker_size,
                diff_algorithm: None,
                ignore_all_space: false,
                ignore_space_change: false,
                ignore_space_at_eol: false,
                ignore_cr_at_eol: false,
            })?
        }
        _ => return Ok(None),
    };
    Ok(Some(String::from_utf8_lossy(&out.content).to_string()))
}

fn try_replay_merge(
    _repo: &Repository,
    path: &str,
    cur: &[u8],
    preimage: &[u8],
    postimage: &[u8],
) -> Option<Vec<u8>> {
    let marker_size = conflict_marker_size(path);
    // Git normalizes the worktree conflict (`thisimage`) before replay; mirror that here.
    let cur_str = std::str::from_utf8(cur).ok()?;
    let cur_norm = normalize_conflicts(cur_str, marker_size).ok()?;
    // When the normalized conflict matches the recorded preimage, the resolution is the postimage
    // (Git's `ll_merge` path; our text merge may not always infer this — t4108 rerere relies on it).
    if cur_norm.as_bytes() == preimage {
        return Some(postimage.to_vec());
    }
    let out = merge(&MergeInput {
        base: preimage,
        ours: cur_norm.as_bytes(),
        theirs: postimage,
        label_ours: "",
        label_base: "",
        label_theirs: "",
        favor: MergeFavor::None,
        style: ConflictStyle::Merge,
        marker_size,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    })
    .ok()?;
    if out.conflicts == 0 {
        Some(out.content)
    } else {
        None
    }
}

fn touch_postimage(git_dir: &Path, id: &RerereId) {
    let p = postimage_path(git_dir, id);
    if let Ok(meta) = fs::metadata(&p) {
        let len = meta.len();
        if let Ok(f) = fs::OpenOptions::new().write(true).truncate(false).open(&p) {
            let _ = f.set_len(len);
        }
    }
}

fn assign_variant(git_dir: &Path, hex: &str) -> i32 {
    let mut v = 0i32;
    loop {
        let id = RerereId {
            hex: hex.to_string(),
            variant: v,
        };
        if !preimage_path(git_dir, &id).exists() && !postimage_path(git_dir, &id).exists() {
            return v;
        }
        v += 1;
    }
}

fn list_complete_variants(git_dir: &Path, hex: &str) -> Vec<i32> {
    let mut out = Vec::new();
    let dir = rr_hex_dir(git_dir, hex);
    if !dir.is_dir() {
        return out;
    }
    let id0 = RerereId {
        hex: hex.to_string(),
        variant: 0,
    };
    if preimage_path(git_dir, &id0).is_file() && postimage_path(git_dir, &id0).is_file() {
        out.push(0);
    }
    let Ok(rd) = fs::read_dir(&dir) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix("preimage.") {
            if let Ok(v) = rest.parse::<i32>() {
                let id = RerereId {
                    hex: hex.to_string(),
                    variant: v,
                };
                if postimage_path(git_dir, &id).is_file() {
                    out.push(v);
                }
            }
        }
    }
    out.sort_unstable();
    out
}

fn thisimage_path(git_dir: &Path, id: &RerereId) -> PathBuf {
    let b = rr_hex_dir(git_dir, &id.hex);
    if id.variant > 0 {
        b.join(format!("thisimage.{}", id.variant))
    } else {
        b.join("thisimage")
    }
}

fn remove_variant(git_dir: &Path, id: &RerereId) {
    let _ = fs::remove_file(postimage_path(git_dir, id));
    let _ = fs::remove_file(preimage_path(git_dir, id));
    let _ = fs::remove_file(thisimage_path(git_dir, id));
}

fn write_preimage_normalized(
    git_dir: &Path,
    id: &RerereId,
    work_content: &str,
    marker_size: usize,
) -> Result<()> {
    let norm = match normalize_conflicts(work_content, marker_size) {
        Ok(s) => s,
        Err(()) => return Ok(()),
    };
    fs::create_dir_all(rr_hex_dir(git_dir, &id.hex))?;
    let pre_path = preimage_path(git_dir, id);
    let post_path = postimage_path(git_dir, id);
    // If the normalized preimage is unchanged, keep an existing postimage (`git apply --3way`
    // re-records the same conflict shape after `merge` + `rerere` resolution).
    let unchanged = fs::read(&pre_path)
        .ok()
        .is_some_and(|existing| existing == norm.as_bytes());
    if !unchanged {
        fs::write(&pre_path, norm.as_bytes())?;
        let _ = fs::remove_file(&post_path);
    }
    Ok(())
}

fn stage_resolved_path(repo: &Repository, index: &mut Index, path: &str) -> Result<()> {
    let wt = repo
        .work_tree
        .as_ref()
        .ok_or_else(|| crate::error::Error::PathError("no work tree".to_string()))?;
    let abs = wt.join(path);
    let data = fs::read(&abs)?;
    let oid = repo.odb.write(ObjectKind::Blob, &data)?;
    let path_b = path.as_bytes().to_vec();
    // Preserve the mode recorded for the conflicting path (stage 2, else stage 3) so
    // executable bits / symlinks survive auto-staging; default to a regular file.
    let mode = index
        .entries
        .iter()
        .find(|e| e.path == path_b && e.stage() == 2)
        .or_else(|| {
            index
                .entries
                .iter()
                .find(|e| e.path == path_b && e.stage() == 3)
        })
        .map(|e| e.mode)
        .unwrap_or(MODE_REGULAR);

    // Fill the stat cache from the on-disk file (mirroring git's `add_index_entry` after a
    // rerere autoupdate) so `git diff-files` sees the resolved path as clean (t3504). Without
    // a fresh stat cache the index entry would carry the stale/zero stat of the conflict
    // stage and `diff-files` would report the file as modified.
    let mut entry = match fs::symlink_metadata(&abs) {
        Ok(meta) => entry_from_metadata(&meta, &path_b, oid, mode),
        Err(_) => IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: data.len().min(u32::MAX as usize) as u32,
            oid,
            flags: path_b.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path_b.clone(),
            base_index_pos: 0,
        },
    };
    entry.flags &= !0x3000;
    index.stage_file(entry);
    Ok(())
}

/// Invoked after mergy operations with conflicts (`merge`, `rebase`, …).
pub fn repo_rerere(repo: &Repository, autoupdate: RerereAutoupdate) -> Result<Vec<RerereEvent>> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    if !rerere_enabled(&config, &repo.git_dir) {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    let autoupdate_on = autoupdate_flag(&config, autoupdate);
    let wt = match &repo.work_tree {
        Some(w) => w,
        None => return Ok(events),
    };

    let index_path = repo.git_dir.join("index");
    if !index_path.exists() {
        return Ok(events);
    }
    let mut index = repo.load_index_at(&index_path)?;
    let mut merge_rr = read_merge_rr(&repo.git_dir)?;

    fs::create_dir_all(rr_root(&repo.git_dir))?;

    // Match Git `do_plain_rerere` first loop: `handle_file` reads the working tree and hashes
    // conflict markers (same path as `find_conflict` three-way conflicts).
    let conflicts = find_three_way_conflicts(&index);
    for path in &conflicts {
        let file_path = wt.join(path);
        let work_content = if file_path.exists() {
            fs::read_to_string(&file_path).unwrap_or_default()
        } else {
            String::new()
        };
        let marker_size = conflict_marker_size(path);
        let algo = repo.odb.hash_algo();
        let digest_len = algo.len();
        let mut hash = [0u8; 32];
        let ret = handle_path(
            &work_content,
            marker_size,
            algo,
            Some(&mut hash[..digest_len]),
        );
        // Mirror `git rerere` `do_plain_rerere`: only evict MERGE_RR when the scan fails (`ret !=
        // 0`). `ret == 0` means the working tree no longer has markers (user resolved) — keep the
        // existing MERGE_RR row so the second phase can write `postimage`.
        if ret != 0 {
            merge_rr.remove(path);
        }
        if ret < 1 {
            continue;
        }
        let hex = hex::encode(&hash[..digest_len]);
        merge_rr.insert(
            path.clone(),
            MergeRrEntry {
                id: Some(RerereId { hex, variant: -1 }),
            },
        );
    }

    let paths: Vec<String> = merge_rr.keys().cloned().collect();
    let mut to_stage: Vec<String> = Vec::new();

    for path in paths {
        let Some(ent) = merge_rr.get_mut(&path) else {
            continue;
        };
        let Some(mut id) = ent.id.clone() else {
            continue;
        };

        let file_path = wt.join(&path);
        let work_content = if file_path.exists() {
            fs::read_to_string(&file_path).unwrap_or_default()
        } else {
            String::new()
        };
        let marker_size = conflict_marker_size(&path);

        if id.variant < 0 {
            let hex = id.hex.clone();
            let mut replayed = false;
            for v in list_complete_variants(&repo.git_dir, &hex) {
                let vid = RerereId {
                    hex: hex.clone(),
                    variant: v,
                };
                let pre = match fs::read(preimage_path(&repo.git_dir, &vid)) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let post = match fs::read(postimage_path(&repo.git_dir, &vid)) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if let Some(res) =
                    try_replay_merge(repo, &path, work_content.as_bytes(), &pre, &post)
                {
                    fs::write(&file_path, &res)?;
                    touch_postimage(&repo.git_dir, &vid);
                    if autoupdate_on {
                        to_stage.push(path.clone());
                    } else {
                        events.push(RerereEvent {
                            path: path.clone(),
                            kind: RerereEventKind::ResolvedFromPrevious,
                        });
                    }
                    replayed = true;
                    break;
                }
            }
            if replayed {
                ent.id = None;
                continue;
            }
            id.variant = assign_variant(&repo.git_dir, &id.hex);
            write_preimage_normalized(&repo.git_dir, &id, &work_content, marker_size)?;
            ent.id = Some(id);
            events.push(RerereEvent {
                path: path.clone(),
                kind: RerereEventKind::RecordedPreimage,
            });
            continue;
        }

        if handle_path(&work_content, marker_size, repo.odb.hash_algo(), None) == 0 {
            fs::copy(&file_path, postimage_path(&repo.git_dir, &id))?;
            events.push(RerereEvent {
                path: path.clone(),
                kind: RerereEventKind::RecordedResolution,
            });
            ent.id = None;
            continue;
        }

        let hex = id.hex.clone();
        let mut replayed = false;
        let synth_bytes: Option<Vec<u8>> =
            synthesize_conflict_from_index(repo, &index, path.as_str())?.map(|s| s.into_bytes());
        let work_bytes = work_content.as_bytes();
        let mut cur_candidates: Vec<&[u8]> = vec![work_bytes];
        if let Some(ref s) = synth_bytes {
            if s.as_slice() != work_bytes {
                cur_candidates.push(s.as_slice());
            }
        }
        // Prefer the working tree; fall back to the in-core conflict from index stages when
        // marker labels differ but the rerere conflict id still matches.
        let mut try_cur = |cur: &[u8]| -> bool {
            for v in list_complete_variants(&repo.git_dir, &hex) {
                let vid = RerereId {
                    hex: hex.clone(),
                    variant: v,
                };
                let pre = match fs::read(preimage_path(&repo.git_dir, &vid)) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let post = match fs::read(postimage_path(&repo.git_dir, &vid)) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if let Some(res) = try_replay_merge(repo, &path, cur, &pre, &post) {
                    let _ = fs::write(&file_path, &res);
                    touch_postimage(&repo.git_dir, &vid);
                    if id.variant != v {
                        remove_variant(&repo.git_dir, &id);
                    }
                    if autoupdate_on {
                        to_stage.push(path.clone());
                    } else {
                        events.push(RerereEvent {
                            path: path.clone(),
                            kind: RerereEventKind::ResolvedFromPrevious,
                        });
                    }
                    return true;
                }
            }
            false
        };
        for cur in cur_candidates {
            if try_cur(cur) {
                replayed = true;
                break;
            }
        }
        if replayed {
            ent.id = None;
            continue;
        }

        id.variant = assign_variant(&repo.git_dir, &id.hex);
        write_preimage_normalized(&repo.git_dir, &id, &work_content, marker_size)?;
        ent.id = Some(id);
        events.push(RerereEvent {
            path: path.clone(),
            kind: RerereEventKind::RecordedPreimage,
        });
    }

    merge_rr.retain(|_, e| e.id.is_some());
    write_merge_rr(&repo.git_dir, &merge_rr)?;

    if !to_stage.is_empty() {
        for p in &to_stage {
            stage_resolved_path(repo, &mut index, p)?;
        }
        repo.write_index(&mut index)?;
        for p in &to_stage {
            events.push(RerereEvent {
                path: p.clone(),
                kind: RerereEventKind::StagedFromPrevious,
            });
        }
    }

    Ok(events)
}

/// After successful commit: record postimages, clear `MERGE_RR` entries.
pub fn rerere_post_commit(repo: &Repository) -> Result<Vec<RerereEvent>> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    if !rerere_enabled(&config, &repo.git_dir) {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    let wt = match &repo.work_tree {
        Some(w) => w,
        None => return Ok(events),
    };
    let index_path = repo.git_dir.join("index");
    if !index_path.exists() {
        return Ok(events);
    }
    let mut merge_rr = read_merge_rr(&repo.git_dir)?;
    if merge_rr.is_empty() {
        return Ok(events);
    }

    let paths: Vec<String> = merge_rr.keys().cloned().collect();
    for path in paths {
        let Some(ent) = merge_rr.get_mut(&path) else {
            continue;
        };
        let Some(id) = ent.id.clone() else {
            continue;
        };
        if id.variant < 0 {
            continue;
        }
        let pre = preimage_path(&repo.git_dir, &id);
        let post = postimage_path(&repo.git_dir, &id);
        if !pre.exists() || post.exists() {
            continue;
        }
        let fp = wt.join(&path);
        if !fp.exists() {
            continue;
        }
        let content = fs::read_to_string(&fp)?;
        let marker_size = conflict_marker_size(&path);
        if handle_path(&content, marker_size, repo.odb.hash_algo(), None) != 0 {
            continue;
        }
        fs::write(&post, content.as_bytes())?;
        events.push(RerereEvent {
            path: path.clone(),
            kind: RerereEventKind::RecordedResolution,
        });
        ent.id = None;
    }

    merge_rr.retain(|_, e| e.id.is_some());
    write_merge_rr(&repo.git_dir, &merge_rr)?;
    Ok(events)
}

/// `git rerere clear` — drop unresolved preimages tracked in `MERGE_RR`, remove `MERGE_RR`.
pub fn rerere_clear(git_dir: &Path) -> Result<()> {
    let merge_rr = read_merge_rr(git_dir)?;
    for ent in merge_rr.values() {
        let Some(id) = &ent.id else {
            continue;
        };
        let post = postimage_path(git_dir, id);
        if post.exists() {
            continue;
        }
        let dir = rr_hex_dir(git_dir, &id.hex);
        if dir.is_dir() {
            let _ = fs::remove_dir_all(&dir);
        }
    }
    let _ = fs::remove_file(merge_rr_path(git_dir));
    Ok(())
}

fn parse_expiry_days_now(config: &ConfigSet, key: &str, now: i64) -> Option<i64> {
    let s = config.get(key)?;
    if let Ok(days) = s.parse::<i64>() {
        return Some(now - days * 86400);
    }
    let tl = s.trim().to_ascii_lowercase();
    if tl == "now" {
        return Some(now);
    }
    let parts: Vec<&str> = tl.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 3 && parts[parts.len() - 2] == "ago" {
        let unit = parts[parts.len() - 1];
        let mut n: i64 = 0;
        for p in &parts[..parts.len() - 2] {
            if let Ok(v) = p.parse::<i64>() {
                n = v;
                break;
            }
        }
        let mult = match unit {
            "second" | "seconds" => 1,
            "minute" | "minutes" => 60,
            "hour" | "hours" => 3600,
            "day" | "days" => 86400,
            "week" | "weeks" => 7 * 86400,
            _ => return None,
        };
        return Some(now - n * mult);
    }
    None
}

/// `git rerere gc`
pub fn rerere_gc(git_dir: &Path) -> Result<()> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(git_dir),
        true,
    )?;
    if !rerere_enabled(&config, git_dir) {
        return Ok(());
    }
    let now: i64 = crate::git_date::tm::get_time_sec();
    let mut cutoff_resolve = now - 60 * 86400;
    let mut cutoff_unresolved = now - 15 * 86400;
    if let Some(c) = parse_expiry_days_now(&config, "gc.rerereresolved", now) {
        cutoff_resolve = c;
    }
    if let Some(c) = parse_expiry_days_now(&config, "gc.rerereunresolved", now) {
        cutoff_unresolved = c;
    }

    let cache = rr_root(git_dir);
    if !cache.is_dir() {
        return Ok(());
    }
    let mut empty_dirs: Vec<String> = Vec::new();
    for entry in fs::read_dir(&cache)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() != 40 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let hex = name;
        let dir = entry.path();
        let mut variants: BTreeSet<i32> = BTreeSet::new();
        for f in fs::read_dir(&dir)? {
            let f = f?;
            let n = f.file_name().to_string_lossy().to_string();
            if n == "preimage" {
                variants.insert(0);
            } else if let Some(r) = n.strip_prefix("preimage.") {
                if let Ok(v) = r.parse() {
                    variants.insert(v);
                }
            }
        }
        for v in variants {
            let id = RerereId {
                hex: hex.clone(),
                variant: v,
            };
            let post = postimage_path(git_dir, &id);
            let pre = preimage_path(git_dir, &id);
            let check = if post.exists() { &post } else { &pre };
            if !check.exists() {
                continue;
            }
            let mtime = fs::metadata(check)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let cutoff = if post.exists() {
                cutoff_resolve
            } else {
                cutoff_unresolved
            };
            if mtime < cutoff {
                remove_variant(git_dir, &id);
            }
        }
        let now_empty = fs::read_dir(&dir).map(|d| d.count() == 0).unwrap_or(true);
        if now_empty {
            empty_dirs.push(hex);
        }
    }
    for hex in empty_dirs {
        let _ = fs::remove_dir(rr_hex_dir(git_dir, &hex));
    }
    Ok(())
}

/// Lines for `git rerere status` (paths listed in `MERGE_RR`).
pub fn rerere_status_lines(repo: &Repository) -> Result<Vec<String>> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    if !rerere_enabled(&config, &repo.git_dir) {
        return Ok(Vec::new());
    }
    let mm = read_merge_rr(&repo.git_dir)?;
    Ok(mm.keys().cloned().collect())
}

/// Unified diff: recorded preimage vs working tree (Git/xdiff style header).
pub fn rerere_diff_for_path(repo: &Repository, path: &str) -> Result<Option<String>> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    if !rerere_enabled(&config, &repo.git_dir) {
        return Ok(None);
    }
    let mm = read_merge_rr(&repo.git_dir)?;
    let ent = match mm.get(path) {
        Some(e) => e,
        None => return Ok(None),
    };
    let Some(id) = &ent.id else {
        return Ok(None);
    };
    let pre_path = preimage_path(&repo.git_dir, id);
    if !pre_path.exists() {
        return Ok(None);
    }
    let recorded = fs::read_to_string(&pre_path)?;
    let wt = repo.work_tree.as_ref();
    let Some(wt) = wt else {
        return Ok(None);
    };
    let cur = fs::read_to_string(wt.join(path)).unwrap_or_default();
    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    for diff in similar::TextDiff::from_lines(&recorded, &cur)
        .unified_diff()
        .context_radius(3)
        .iter_hunks()
    {
        out.push_str(&format!("{diff}"));
    }
    Ok(Some(out))
}

/// Paths still needing resolution, matching Git's `rerere_remaining`
/// (`builtin/rerere.c` "remaining"): start from the MERGE_RR list (the
/// conflicts rerere is tracking), then walk the index. An index conflict that
/// has been RESOLVED (no unmerged stages) marks the matching MERGE_RR entry as
/// resolved; a PUNTED conflict (unmerged but not rerere-handled, e.g.
/// rename/rename) is inserted into the list. Finally emit every MERGE_RR entry
/// that was not marked resolved.
pub fn rerere_remaining_lines(repo: &Repository) -> Result<Vec<String>> {
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    if !rerere_enabled(&config, &repo.git_dir) {
        return Ok(Vec::new());
    }
    let index_path = repo.git_dir.join("index");
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let index = repo.load_index_at(&index_path)?;

    // Tracked conflicts come from MERGE_RR; map path -> resolved?.
    let merge_rr = read_merge_rr(&repo.git_dir)?;
    // Preserve the MERGE_RR insertion/sorted order for output.
    let mut entries: Vec<(String, bool)> = merge_rr.keys().map(|p| (p.clone(), false)).collect();
    let mut index_of: BTreeMap<String, usize> = BTreeMap::new();
    for (i, (p, _)) in entries.iter().enumerate() {
        index_of.insert(p.clone(), i);
    }

    let mut i = 0usize;
    while i < index.entries.len() {
        let path = String::from_utf8_lossy(&index.entries[i].path).to_string();
        let (next, ty) = check_one_conflict(&index, i);
        match ty {
            // PUNTED: an unmerged path rerere does not track (e.g. rename/rename)
            // — surface it as remaining.
            1 => {
                if let Some(&idx) = index_of.get(&path) {
                    entries[idx].1 = false;
                } else {
                    index_of.insert(path.clone(), entries.len());
                    entries.push((path, false));
                }
            }
            // RESOLVED: the index has no unmerged stages for this path, so the
            // tracked conflict (if any) is now resolved.
            0 => {
                if let Some(&idx) = index_of.get(&path) {
                    entries[idx].1 = true;
                }
            }
            _ => {}
        }
        i = next;
    }

    Ok(entries
        .into_iter()
        .filter(|(_, resolved)| !*resolved)
        .map(|(p, _)| p)
        .collect())
}

/// Drop recorded resolution for `path` (working tree must show conflict markers).
pub fn rerere_forget_path(repo: &Repository, path: &str) -> Result<Vec<RerereEvent>> {
    let mut events = Vec::new();
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    if !rerere_enabled(&config, &repo.git_dir) {
        return Ok(events);
    }
    let wt = repo
        .work_tree
        .as_ref()
        .ok_or_else(|| crate::error::Error::PathError("no work tree".to_string()))?;
    let path = {
        let cwd = crate::environment::Environment::capture_process()
            .cwd
            .clone();
        if let Ok(prefix) = cwd.strip_prefix(wt) {
            let prefix = prefix.to_string_lossy().replace('\\', "/");
            if prefix.is_empty() {
                path.to_string()
            } else {
                format!("{prefix}/{path}")
            }
        } else {
            path.to_string()
        }
    };
    let path = path.as_str();
    let fp = wt.join(path);
    if !fp.exists() {
        return Err(crate::error::Error::PathError(format!(
            "no such path '{path}' in the working tree"
        )));
    }
    let index_path = repo.git_dir.join("index");
    let index = repo.load_index_at(&index_path)?;
    let has_base_stage = index
        .entries
        .iter()
        .any(|e| e.path == path.as_bytes() && e.stage() == 1)
        || index
            .resolve_undo
            .as_ref()
            .and_then(|records| records.get(path.as_bytes()))
            .is_some_and(|record| record.modes[0] != 0);
    if !has_base_stage {
        events.push(RerereEvent {
            path: path.to_string(),
            kind: RerereEventKind::NoRemembered,
        });
        return Ok(events);
    }
    let synth = match synthesize_conflict_from_index(repo, &index, path)? {
        Some(s) => s,
        None => {
            return Err(crate::error::Error::PathError(format!(
                "could not parse conflict hunks in '{path}'"
            )));
        }
    };
    let marker_size = conflict_marker_size(path);
    let algo = repo.odb.hash_algo();
    let digest_len = algo.len();
    let mut hash = [0u8; 32];
    if handle_path(&synth, marker_size, algo, Some(&mut hash[..digest_len])) != 1 {
        return Err(crate::error::Error::PathError(format!(
            "could not parse conflict hunks in '{path}'"
        )));
    }
    let hex = hex::encode(&hash[..digest_len]);
    let work = fs::read_to_string(&fp)?;
    let mut forgot_id: Option<RerereId> = None;
    for v in list_complete_variants(&repo.git_dir, &hex) {
        let id = RerereId {
            hex: hex.clone(),
            variant: v,
        };
        let pre = fs::read(preimage_path(&repo.git_dir, &id)).ok();
        let post = fs::read(postimage_path(&repo.git_dir, &id)).ok();
        let (Some(pre_b), Some(post_b)) = (pre, post) else {
            continue;
        };
        if try_replay_merge(repo, path, work.as_bytes(), &pre_b, &post_b).is_some() {
            let _ = fs::remove_file(postimage_path(&repo.git_dir, &id));
            write_preimage_normalized(&repo.git_dir, &id, &work, marker_size)?;
            events.push(RerereEvent {
                path: path.to_string(),
                kind: RerereEventKind::UpdatedPreimage,
            });
            events.push(RerereEvent {
                path: path.to_string(),
                kind: RerereEventKind::Forgot,
            });
            forgot_id = Some(id);
            break;
        }
    }
    let Some(id) = forgot_id else {
        if work.as_bytes().contains(&0) {
            return Ok(events);
        }
        events.push(RerereEvent {
            path: path.to_string(),
            kind: RerereEventKind::NoRemembered,
        });
        return Ok(events);
    };
    let mut merge_rr = read_merge_rr(&repo.git_dir)?;
    merge_rr.insert(path.to_string(), MergeRrEntry { id: Some(id) });
    write_merge_rr(&repo.git_dir, &merge_rr)?;
    Ok(events)
}

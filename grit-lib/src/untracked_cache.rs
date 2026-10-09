//! Git index UNTR (untracked cache) — `git/dir.c` / `read-cache.c`.
#![allow(clippy::too_many_arguments)]

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use crate::config::{parse_path, ConfigSet};
use crate::error::{Error, Result};
use crate::ewah_bitmap::EwahBitmap;
use crate::ignore::IgnoreMatcher;
use crate::index::{Index, MODE_GITLINK};
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::repo::Repository;

pub const DIR_SHOW_OTHER_DIRECTORIES: u32 = 1 << 1;
pub const DIR_HIDE_EMPTY_DIRECTORIES: u32 = 1 << 2;

/// Git `struct stat_data` on disk (36 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatDataDisk {
    pub ctime_sec: u32,
    pub ctime_nsec: u32,
    pub mtime_sec: u32,
    pub mtime_nsec: u32,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

const STAT_DATA_LEN: usize = 36;

impl StatDataDisk {
    fn to_bytes(self) -> [u8; STAT_DATA_LEN] {
        let mut out = [0u8; STAT_DATA_LEN];
        out[0..4].copy_from_slice(&self.ctime_sec.to_be_bytes());
        out[4..8].copy_from_slice(&self.ctime_nsec.to_be_bytes());
        out[8..12].copy_from_slice(&self.mtime_sec.to_be_bytes());
        out[12..16].copy_from_slice(&self.mtime_nsec.to_be_bytes());
        out[16..20].copy_from_slice(&self.dev.to_be_bytes());
        out[20..24].copy_from_slice(&self.ino.to_be_bytes());
        out[24..28].copy_from_slice(&self.uid.to_be_bytes());
        out[28..32].copy_from_slice(&self.gid.to_be_bytes());
        out[32..36].copy_from_slice(&self.size.to_be_bytes());
        out
    }

    fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < STAT_DATA_LEN {
            return None;
        }
        Some(Self {
            ctime_sec: u32::from_be_bytes(b[0..4].try_into().ok()?),
            ctime_nsec: u32::from_be_bytes(b[4..8].try_into().ok()?),
            mtime_sec: u32::from_be_bytes(b[8..12].try_into().ok()?),
            mtime_nsec: u32::from_be_bytes(b[12..16].try_into().ok()?),
            dev: u32::from_be_bytes(b[16..20].try_into().ok()?),
            ino: u32::from_be_bytes(b[20..24].try_into().ok()?),
            uid: u32::from_be_bytes(b[24..28].try_into().ok()?),
            gid: u32::from_be_bytes(b[28..32].try_into().ok()?),
            size: u32::from_be_bytes(b[32..36].try_into().ok()?),
        })
    }
}

#[cfg(unix)]
fn stat_data_from_meta(meta: &fs::Metadata) -> StatDataDisk {
    StatDataDisk {
        ctime_sec: meta.ctime() as u32,
        ctime_nsec: meta.ctime_nsec() as u32,
        mtime_sec: meta.mtime() as u32,
        mtime_nsec: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        size: meta.len() as u32,
    }
}

#[cfg(not(unix))]
fn stat_data_from_meta(meta: &fs::Metadata) -> StatDataDisk {
    StatDataDisk {
        mtime_sec: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0),
        size: meta.len() as u32,
        ..Default::default()
    }
}

#[derive(Clone, Debug)]
pub struct OidStat {
    pub stat: StatDataDisk,
    pub oid: ObjectId,
    pub valid: bool,
}

impl Default for OidStat {
    fn default() -> Self {
        Self {
            stat: StatDataDisk::default(),
            oid: ObjectId::zero(),
            valid: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UntrackedCacheDir {
    pub name: String,
    pub untracked: Vec<String>,
    pub dirs: Vec<UntrackedCacheDir>,
    pub recurse: bool,
    pub check_only: bool,
    pub valid: bool,
    pub exclude_oid: ObjectId,
    pub stat_data: StatDataDisk,
}

impl UntrackedCacheDir {
    fn new(name: String) -> Self {
        Self {
            name,
            untracked: Vec::new(),
            dirs: Vec::new(),
            recurse: false,
            check_only: false,
            valid: false,
            exclude_oid: ObjectId::zero(),
            stat_data: StatDataDisk::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UntrackedCache {
    pub ident: Vec<u8>,
    pub ss_info_exclude: OidStat,
    pub ss_excludes_file: OidStat,
    pub dir_flags: u32,
    pub exclude_per_dir: String,
    pub root: Option<UntrackedCacheDir>,
    pub dir_created: u64,
    pub gitignore_invalidated: u64,
    pub dir_invalidated: u64,
    pub dir_opened: u64,
}

impl UntrackedCache {
    pub fn new_shell(dir_flags: u32, ident: Vec<u8>) -> Self {
        Self {
            ident,
            ss_info_exclude: OidStat::default(),
            ss_excludes_file: OidStat::default(),
            dir_flags,
            exclude_per_dir: ".gitignore".to_string(),
            root: None,
            dir_created: 0,
            gitignore_invalidated: 0,
            dir_invalidated: 0,
            dir_opened: 0,
        }
    }

    pub fn reset_stats(&mut self) {
        self.dir_created = 0;
        self.gitignore_invalidated = 0;
        self.dir_invalidated = 0;
        self.dir_opened = 0;
    }
}

fn encode_varint(mut value: u64, buf: &mut Vec<u8>) {
    let mut varint = [0u8; 16];
    let mut pos = varint.len() - 1;
    varint[pos] = (value & 127) as u8;
    while {
        value >>= 7;
        value != 0
    } {
        pos -= 1;
        value -= 1;
        varint[pos] = 128 | ((value & 127) as u8);
    }
    buf.extend_from_slice(&varint[pos..]);
}

fn decode_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    if bytes.is_empty() {
        return None;
    }
    let mut i = 0usize;
    let mut c = bytes[i];
    i += 1;
    let mut val = (c & 127) as u64;
    while c & 128 != 0 {
        if i >= bytes.len() {
            return None;
        }
        c = bytes[i];
        i += 1;
        val = ((val + 1) << 7) + (c & 127) as u64;
    }
    Some((val, i))
}

struct WriteDirCtx<'a> {
    index: &'a mut usize,
    valid: EwahBitmap,
    check_only: EwahBitmap,
    sha1_valid: EwahBitmap,
    out: Vec<u8>,
    sb_stat: Vec<u8>,
    sb_sha1: Vec<u8>,
}

fn write_one_dir(ucd: &UntrackedCacheDir, wd: &mut WriteDirCtx<'_>) {
    let i = *wd.index;
    *wd.index += 1;

    let mut ucd = ucd.clone();
    if !ucd.valid {
        ucd.untracked.clear();
        ucd.check_only = false;
    }

    if ucd.check_only {
        wd.check_only.set_bit_extend(i);
    }
    if ucd.valid {
        wd.valid.set_bit_extend(i);
        wd.sb_stat.extend_from_slice(&ucd.stat_data.to_bytes());
    }
    if !ucd.exclude_oid.is_zero() {
        wd.sha1_valid.set_bit_extend(i);
        wd.sb_sha1.extend_from_slice(ucd.exclude_oid.as_bytes());
    }

    ucd.untracked.sort();
    encode_varint(ucd.untracked.len() as u64, &mut wd.out);

    let recurse_count = ucd.dirs.iter().filter(|d| d.recurse).count() as u64;
    encode_varint(recurse_count, &mut wd.out);

    wd.out.extend_from_slice(ucd.name.as_bytes());
    wd.out.push(0);

    for n in &ucd.untracked {
        wd.out.extend_from_slice(n.as_bytes());
        wd.out.push(0);
    }

    let mut subdirs: Vec<_> = ucd.dirs.iter().filter(|d| d.recurse).collect();
    subdirs.sort_by(|a, b| a.name.cmp(&b.name));
    for d in subdirs {
        write_one_dir(d, wd);
    }
}

/// Serialize UNTR payload (extension body only, no signature header).
pub fn write_untracked_extension(uc: &UntrackedCache) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(uc.ident.len() as u64, &mut out);
    out.extend_from_slice(&uc.ident);

    let mut hdr = Vec::with_capacity(STAT_DATA_LEN * 2 + 4);
    hdr.extend_from_slice(&uc.ss_info_exclude.stat.to_bytes());
    hdr.extend_from_slice(&uc.ss_excludes_file.stat.to_bytes());
    hdr.extend_from_slice(&uc.dir_flags.to_be_bytes());
    out.extend_from_slice(&hdr);
    out.extend_from_slice(uc.ss_info_exclude.oid.as_bytes());
    out.extend_from_slice(uc.ss_excludes_file.oid.as_bytes());
    out.extend_from_slice(uc.exclude_per_dir.as_bytes());
    out.push(0);

    let Some(root) = &uc.root else {
        encode_varint(0, &mut out);
        return out;
    };

    let mut wd = WriteDirCtx {
        index: &mut 0,
        valid: EwahBitmap::new(),
        check_only: EwahBitmap::new(),
        sha1_valid: EwahBitmap::new(),
        out: Vec::new(),
        sb_stat: Vec::new(),
        sb_sha1: Vec::new(),
    };
    let mut sorted_root = root.clone();
    sorted_root.untracked.sort();
    sorted_root.dirs.sort_by(|a, b| a.name.cmp(&b.name));
    write_one_dir(&sorted_root, &mut wd);

    encode_varint(*wd.index as u64, &mut out);
    out.append(&mut wd.out);

    // Match Git `write_untracked_extension`: valid, check_only, sha1_valid (`dir.c`).
    let mut tmp = Vec::new();
    wd.valid.serialize(&mut tmp);
    out.append(&mut tmp);
    tmp.clear();
    wd.check_only.serialize(&mut tmp);
    out.append(&mut tmp);
    tmp.clear();
    wd.sha1_valid.serialize(&mut tmp);
    out.append(&mut tmp);
    out.append(&mut wd.sb_stat);
    out.append(&mut wd.sb_sha1);
    out.push(0);
    out
}

/// Parse UNTR body (after 4-byte signature + 4-byte size).
pub fn parse_untracked_extension(data: &[u8]) -> Option<UntrackedCache> {
    if data.len() <= 1 || data[data.len() - 1] != 0 {
        return None;
    }
    let end = data.len() - 1;
    let data = &data[..end];

    let (ident_len, c) = decode_varint(data)?;
    let start = c;
    if start + ident_len as usize > data.len() {
        return None;
    }
    let ident = data[start..start + ident_len as usize].to_vec();
    let mut pos = start + ident_len as usize;

    const HDR: usize = STAT_DATA_LEN * 2 + 4;
    if data.len() < pos + HDR + 40 {
        return None;
    }
    let info_stat = StatDataDisk::from_bytes(&data[pos..])?;
    pos += STAT_DATA_LEN;
    let excl_stat = StatDataDisk::from_bytes(&data[pos..])?;
    pos += STAT_DATA_LEN;
    let dir_flags = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?);
    pos += 4;
    let oid_info = ObjectId::from_bytes(&data[pos..pos + 20]).ok()?;
    pos += 20;
    let oid_excl = ObjectId::from_bytes(&data[pos..pos + 20]).ok()?;
    pos += 20;

    let eos = data[pos..].iter().position(|&b| b == 0)?;
    let exclude_per_dir = String::from_utf8(data[pos..pos + eos].to_vec()).ok()?;
    pos += eos + 1;

    let mut uc = UntrackedCache {
        ident,
        ss_info_exclude: OidStat {
            stat: info_stat,
            oid: oid_info,
            valid: true,
        },
        ss_excludes_file: OidStat {
            stat: excl_stat,
            oid: oid_excl,
            valid: true,
        },
        dir_flags,
        exclude_per_dir,
        root: None,
        dir_created: 0,
        gitignore_invalidated: 0,
        dir_invalidated: 0,
        dir_opened: 0,
    };

    if pos >= data.len() {
        return Some(uc);
    }
    let (n_nodes, c) = decode_varint(&data[pos..])?;
    pos += c;
    if n_nodes == 0 {
        return Some(uc);
    }

    fn read_one_dir(data: &[u8], pos: &mut usize) -> Option<UntrackedCacheDir> {
        let (untracked_nr, c) = decode_varint(&data[*pos..])?;
        *pos += c;
        let (dirs_nr, c) = decode_varint(&data[*pos..])?;
        *pos += c;
        let untracked_nr = untracked_nr as usize;
        let dirs_nr = dirs_nr as usize;

        let name_start = *pos;
        let name_end = name_start + data[name_start..].iter().position(|&b| b == 0)?;
        let name = String::from_utf8(data[name_start..name_end].to_vec()).ok()?;
        *pos = name_end + 1;

        let mut untracked = Vec::with_capacity(untracked_nr);
        for _ in 0..untracked_nr {
            let s = *pos;
            let e = s + data[s..].iter().position(|&b| b == 0)?;
            untracked.push(String::from_utf8(data[s..e].to_vec()).ok()?);
            *pos = e + 1;
        }

        let mut ucd = UntrackedCacheDir::new(name);
        ucd.untracked = untracked;

        for _ in 0..dirs_nr {
            ucd.dirs.push(read_one_dir(data, pos)?);
        }
        Some(ucd)
    }

    let mut read_pos = pos;
    let mut root = read_one_dir(data, &mut read_pos)?;

    let rest = &data[read_pos..];
    let (valid_bm, vlen) = EwahBitmap::deserialize_prefix(rest)?;
    let rest = &rest[vlen..];
    let (check_bm, clen) = EwahBitmap::deserialize_prefix(rest)?;
    let rest = &rest[clen..];
    let (sha_bm, slen) = EwahBitmap::deserialize_prefix(rest)?;
    let rest = &rest[slen..];

    let n = n_nodes as usize;
    let mut check_bits = Vec::new();
    check_bm.each_set_bit(|i| check_bits.push(i));
    let mut valid_bits = Vec::new();
    valid_bm.each_set_bit(|i| valid_bits.push(i));
    let mut sha_bits = Vec::new();
    sha_bm.each_set_bit(|i| sha_bits.push(i));

    let stat_len = valid_bits.len() * STAT_DATA_LEN;
    let oid_len = sha_bits.len() * 20;
    if rest.len() < stat_len + oid_len {
        return None;
    }
    let (stat_part, tail) = rest.split_at(stat_len);
    let (oid_part, after_oids) = tail.split_at(oid_len);
    if !after_oids.is_empty() {
        return None;
    }
    let mut stat_slice = stat_part;
    let mut oid_slice = oid_part;

    fn apply(
        u: &mut UntrackedCacheDir,
        idx: &mut usize,
        check: &[usize],
        valid: &[usize],
        sha: &[usize],
        stat_bytes: &mut &[u8],
        oid_bytes: &mut &[u8],
    ) -> Option<()> {
        let i = *idx;
        *idx += 1;
        u.recurse = true;
        u.check_only = check.contains(&i);
        if valid.contains(&i) {
            u.valid = true;
            if stat_bytes.len() < STAT_DATA_LEN {
                return None;
            }
            u.stat_data = StatDataDisk::from_bytes(&stat_bytes[..STAT_DATA_LEN])?;
            *stat_bytes = &stat_bytes[STAT_DATA_LEN..];
        }
        if sha.contains(&i) {
            if oid_bytes.len() < 20 {
                return None;
            }
            u.exclude_oid = ObjectId::from_bytes(&oid_bytes[..20]).ok()?;
            *oid_bytes = &oid_bytes[20..];
        }
        u.dirs.sort_by(|a, b| a.name.cmp(&b.name));
        for d in &mut u.dirs {
            apply(d, idx, check, valid, sha, stat_bytes, oid_bytes)?;
        }
        Some(())
    }

    let mut idx = 0usize;
    apply(
        &mut root,
        &mut idx,
        &check_bits,
        &valid_bits,
        &sha_bits,
        &mut stat_slice,
        &mut oid_slice,
    )?;
    if idx != n {
        return None;
    }
    uc.root = Some(root);
    Some(uc)
}

pub fn untracked_cache_ident(work_tree: &Path) -> Vec<u8> {
    #[cfg(unix)]
    let sysname = match nix::sys::utsname::uname() {
        Ok(uts) => uts.sysname().to_string_lossy().into_owned(),
        Err(_) => "unknown".to_string(),
    };
    #[cfg(not(unix))]
    let sysname = "unknown".to_string();

    let loc = work_tree.display().to_string();
    let mut s = format!("Location {loc}, system {sysname}");
    s.push('\0');
    s.into_bytes()
}

pub fn dir_flags_from_config(config: &ConfigSet) -> u32 {
    if config
        .get("status.showUntrackedFiles")
        .or_else(|| config.get("status.showuntrackedfiles"))
        .is_some_and(|v| v.eq_ignore_ascii_case("all"))
    {
        0
    } else {
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES
    }
}

fn global_excludes_path(repo: &Repository, config: &ConfigSet) -> Option<PathBuf> {
    let raw = config
        .get("core.excludesFile")
        .or_else(|| config.get("core.excludesfile"))?;
    let expanded = parse_path(repo.environment(), &raw);
    let p = Path::new(&expanded);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        repo.work_tree.as_ref().map(|wt| wt.join(p))
    }
}

fn file_stat_and_blob_oid(path: &Path, algo: HashAlgo) -> Result<(StatDataDisk, ObjectId)> {
    match fs::metadata(path) {
        Ok(meta) => {
            let st = stat_data_from_meta(&meta);
            let mut f = fs::File::open(path).map_err(Error::Io)?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(Error::Io)?;
            let oid = if buf.is_empty() {
                algo.hash_object(ObjectKind::Blob, &buf)
            } else {
                // Match Git's exclude-file oid normalization used by the untracked cache:
                // parsed non-empty ignore files carry a trailing newline sentinel.
                let mut normalized = buf;
                normalized.push(b'\n');
                algo.hash_object(ObjectKind::Blob, &normalized)
            };
            Ok((st, oid))
        }
        Err(_) => Ok((StatDataDisk::default(), ObjectId::zero())),
    }
}

fn do_invalidate_gitignore(dir: &mut UntrackedCacheDir) {
    dir.valid = false;
    dir.untracked.clear();
    for d in &mut dir.dirs {
        do_invalidate_gitignore(d);
    }
}

fn invalidate_gitignore(uc: &mut UntrackedCache) {
    if let Some(root) = uc.root.as_mut() {
        do_invalidate_gitignore(root);
    }
}

fn invalidate_directory(uc: &mut UntrackedCache, dir: &mut UntrackedCacheDir) {
    if dir.valid {
        uc.dir_invalidated += 1;
    }
    dir.valid = false;
    dir.untracked.clear();
    for d in &mut dir.dirs {
        // Preserve collapsed placeholders across parent invalidation so their
        // cache nodes remain available for dump-shape parity on the next scan.
        d.recurse = d.check_only;
    }
}

fn tracked_ignore_blob_oid(index: &Index, rel_path: &str) -> Option<ObjectId> {
    let entry = index.get(rel_path.as_bytes(), 0)?;
    if entry.mode == MODE_GITLINK {
        return None;
    }
    Some(entry.oid)
}

fn invalidate_one_directory_for_path(uc: &mut UntrackedCache, dir: &mut UntrackedCacheDir) {
    if dir.valid {
        uc.dir_invalidated += 1;
    }
    dir.valid = false;
    dir.untracked.clear();
    for d in &mut dir.dirs {
        if d.check_only {
            d.recurse = true;
        }
    }
}

pub fn invalidate_path(uc: &mut UntrackedCache, path: &str) {
    let Some(mut root) = uc.root.take() else {
        return;
    };
    let _ = invalidate_one_component(uc, &mut root, path);
    uc.root = Some(root);
}

fn invalidate_one_component(
    uc: &mut UntrackedCache,
    dir: &mut UntrackedCacheDir,
    path: &str,
) -> bool {
    if let Some(slash) = path.find('/') {
        let (comp, tail) = path.split_at(slash);
        let tail = &tail[1..];
        if let Some(d) = dir.dirs.iter_mut().find(|x| x.name == comp) {
            let ret = invalidate_one_component(uc, d, tail);
            if ret {
                invalidate_one_directory_for_path(uc, dir);
            }
            ret
        } else {
            false
        }
    } else {
        invalidate_one_directory_for_path(uc, dir);
        uc.dir_flags & DIR_SHOW_OTHER_DIRECTORIES != 0
    }
}

fn has_tracked_under(
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    rel_dir: &str,
) -> bool {
    let prefix = if rel_dir.is_empty() {
        String::new()
    } else {
        format!("{rel_dir}/")
    };
    tracked
        .range::<String, _>(prefix.clone()..)
        .next()
        .is_some_and(|t| t.starts_with(&prefix))
        || gitlinks.iter().any(|g| {
            g.as_str() == rel_dir || (!rel_dir.is_empty() && g.starts_with(&format!("{rel_dir}/")))
        })
}

fn has_hidden_untracked_file_or_dir(
    repo: &Repository,
    index: &Index,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    rel: &str,
    abs: &Path,
    uc: &mut UntrackedCache,
) -> Result<bool> {
    let entries = match fs::read_dir(abs) {
        Ok(e) => {
            uc.dir_opened += 1;
            e
        }
        Err(_) => return Ok(false),
    };
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());
    for entry in sorted {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let child_rel = relative_path(rel, &name);
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir && gitlinks.contains(&child_rel) {
            continue;
        }
        if tracked.contains(&child_rel) {
            continue;
        }
        if is_dir {
            if has_hidden_untracked_file_or_dir(
                repo, index, tracked, gitlinks, matcher, &child_rel, &path, uc,
            )? {
                return Ok(true);
            }
        } else {
            let (is_ign, _) = matcher.check_path(repo, Some(index), &child_rel, false)?;
            if !is_ign && name.starts_with('.') {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn has_ignored_entry_or_dir(
    repo: &Repository,
    index: &Index,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    rel: &str,
    abs: &Path,
) -> Result<bool> {
    if matcher.check_path(repo, Some(index), rel, true)?.0 {
        return Ok(true);
    }
    let entries = match fs::read_dir(abs) {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());
    for entry in sorted {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let child_rel = relative_path(rel, &name);
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir && gitlinks.contains(&child_rel) {
            continue;
        }
        if tracked.contains(&child_rel) {
            continue;
        }
        if is_dir {
            if has_ignored_entry_or_dir(repo, index, tracked, gitlinks, matcher, &child_rel, &path)?
            {
                return Ok(true);
            }
        } else {
            let (is_ign, _) = matcher.check_path(repo, Some(index), &child_rel, false)?;
            if is_ign {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn relative_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UntrackedIgnoredMode {
    No,
    Traditional,
    Matching,
}

fn fill_exclude_oids(
    repo: &Repository,
    _work_tree: &Path,
    config: &ConfigSet,
    uc: &mut UntrackedCache,
) -> Result<()> {
    let info_path = repo.git_dir.join("info/exclude");
    let algo = repo.odb.hash_algo();
    let (st_i, oid_i) = file_stat_and_blob_oid(&info_path, algo)?;
    if uc.ss_info_exclude.valid
        && (uc.ss_info_exclude.stat != st_i || uc.ss_info_exclude.oid != oid_i)
    {
        uc.gitignore_invalidated += 1;
        invalidate_gitignore(uc);
    }
    uc.ss_info_exclude.stat = st_i;
    uc.ss_info_exclude.oid = oid_i;
    uc.ss_info_exclude.valid = true;

    let (st_e, oid_e) = if let Some(p) = global_excludes_path(repo, config) {
        file_stat_and_blob_oid(&p, algo)?
    } else {
        (StatDataDisk::default(), ObjectId::zero())
    };
    if uc.ss_excludes_file.valid
        && (uc.ss_excludes_file.stat != st_e || uc.ss_excludes_file.oid != oid_e)
    {
        uc.gitignore_invalidated += 1;
        invalidate_gitignore(uc);
    }
    uc.ss_excludes_file.stat = st_e;
    uc.ss_excludes_file.oid = oid_e;
    uc.ss_excludes_file.valid = true;

    Ok(())
}

fn lookup_or_create_child<'a>(
    parent: &'a mut UntrackedCacheDir,
    name: &str,
    uc: &mut UntrackedCache,
) -> &'a mut UntrackedCacheDir {
    if let Some(i) = parent.dirs.iter().position(|d| d.name == name) {
        return &mut parent.dirs[i];
    }
    uc.dir_created += 1;
    parent.dirs.push(UntrackedCacheDir::new(name.to_string()));
    let n = parent.dirs.len() - 1;
    &mut parent.dirs[n]
}

fn valid_cached_dir(ucd: &UntrackedCacheDir, abs: &Path, check_only: bool) -> bool {
    if !ucd.valid {
        return false;
    }
    let meta = match fs::symlink_metadata(abs) {
        Ok(m) => m,
        Err(_) => return false,
    };
    stat_data_from_meta(&meta) == ucd.stat_data && ucd.check_only == check_only
}

enum DirSource {
    Disk(fs::ReadDir),
    Cache {
        dir_idx: usize,
        file_idx: usize,
        child_dirs: Vec<UntrackedCacheDir>,
        child_files: Vec<String>,
    },
}

/// Refresh untracked cache tree and counters (for `git status`).
pub fn refresh_untracked_cache_for_status(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    config: &ConfigSet,
    uc: &mut UntrackedCache,
    show_all_untracked: bool,
    ignored_mode: UntrackedIgnoredMode,
) -> Result<()> {
    uc.reset_stats();
    let requested_flags = if show_all_untracked {
        0u32
    } else {
        DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES
    };

    let mut mode_switched = false;
    if uc.dir_flags != requested_flags && uc.dir_flags != dir_flags_from_config(config) {
        *uc = UntrackedCache::new_shell(requested_flags, untracked_cache_ident(work_tree));
        mode_switched = true;
    }
    uc.dir_flags = requested_flags;

    fill_exclude_oids(repo, work_tree, config, uc)?;
    if mode_switched {
        uc.gitignore_invalidated += 1;
    }

    let tracked: BTreeSet<String> = index
        .entries
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    let gitlinks: BTreeSet<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode == MODE_GITLINK)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();

    let mut matcher = IgnoreMatcher::from_repository(repo)?;

    if uc.root.is_none() {
        uc.root = Some(UntrackedCacheDir::new(String::new()));
    }
    let mut root = uc
        .root
        .take()
        .ok_or_else(|| Error::IndexError("no uc root".into()))?;

    read_directory_recursive(
        repo,
        index,
        work_tree,
        &tracked,
        &gitlinks,
        &mut matcher,
        ignored_mode,
        show_all_untracked,
        false,
        &mut root,
        "",
        work_tree,
        uc,
    )?;

    uc.root = Some(root);

    Ok(())
}

/// Collect untracked paths from a populated untracked cache tree.
///
/// The returned paths are repository-relative and match the cache shape built by
/// [`refresh_untracked_cache_for_status`], including collapsed `dir/` entries in
/// normal untracked mode and fully expanded file paths in `-uall` mode.
#[must_use]
pub fn collect_untracked_from_cache(uc: &UntrackedCache) -> Vec<String> {
    fn walk(dir: &UntrackedCacheDir, rel: &str, out: &mut Vec<String>) {
        for name in &dir.untracked {
            if rel.is_empty() {
                out.push(name.clone());
            } else {
                out.push(format!("{rel}/{name}"));
            }
        }
        let mut children: Vec<&UntrackedCacheDir> = dir
            .dirs
            .iter()
            .filter(|d| d.recurse && !d.check_only)
            .collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        for child in children {
            let child_rel = if rel.is_empty() {
                child.name.clone()
            } else {
                format!("{rel}/{}", child.name)
            };
            walk(child, &child_rel, out);
        }
    }

    let mut out = Vec::new();
    if let Some(root) = uc.root.as_ref() {
        walk(root, "", &mut out);
    }
    out.sort();
    out
}

fn read_directory_recursive(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: UntrackedIgnoredMode,
    show_all: bool,
    check_only: bool,
    ucd: &mut UntrackedCacheDir,
    rel: &str,
    abs: &Path,
    uc: &mut UntrackedCache,
) -> Result<()> {
    let parent_exclude_rel = if rel.is_empty() {
        ".gitignore".to_string()
    } else {
        format!("{rel}/.gitignore")
    };
    let parent_exclude_path = work_tree.join(&parent_exclude_rel);
    let tracked_ignore_oid = tracked_ignore_blob_oid(index, &parent_exclude_rel);
    let parent_exclude_oid = match fs::metadata(&parent_exclude_path) {
        Ok(_) => {
            if tracked_ignore_oid.is_some() {
                ObjectId::zero()
            } else {
                file_stat_and_blob_oid(&parent_exclude_path, repo.odb.hash_algo())
                    .map(|(_, oid)| oid)
                    .unwrap_or_else(|_| ObjectId::zero())
            }
        }
        Err(_) => tracked_ignore_oid.unwrap_or_else(ObjectId::zero),
    };
    let parent_exclude_changed = parent_exclude_oid != ucd.exclude_oid;
    if ucd.valid && parent_exclude_changed {
        uc.dir_invalidated += 1;
        uc.gitignore_invalidated += 1;
        do_invalidate_gitignore(ucd);
    }

    let use_disk = !valid_cached_dir(ucd, abs, check_only);
    let mut src = if use_disk {
        invalidate_directory(uc, ucd);
        uc.dir_opened += 1;
        let p = if abs == work_tree && rel.is_empty() {
            work_tree.to_path_buf()
        } else {
            abs.to_path_buf()
        };
        DirSource::Disk(fs::read_dir(&p).map_err(Error::Io)?)
    } else {
        let mut child_dirs: Vec<_> = ucd
            .dirs
            .iter()
            .filter(|d| d.recurse && !d.check_only)
            .cloned()
            .collect();
        child_dirs.sort_by(|a, b| a.name.cmp(&b.name));
        let mut child_files = ucd.untracked.clone();
        child_files.sort();
        DirSource::Cache {
            dir_idx: 0,
            file_idx: 0,
            child_dirs,
            child_files,
        }
    };

    ucd.check_only = check_only;

    loop {
        let next = match &mut src {
            DirSource::Disk(rd) => {
                let Some(Ok(entry)) = rd.next() else {
                    break;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    continue;
                }
                let path = entry.path();
                let is_dir = entry.file_type().map_err(Error::Io)?.is_dir();
                Some((name, path, is_dir))
            }
            DirSource::Cache {
                dir_idx,
                file_idx,
                child_dirs,
                child_files,
            } => {
                while *dir_idx < child_dirs.len() && !child_dirs[*dir_idx].recurse {
                    *dir_idx += 1;
                }
                if *dir_idx < child_dirs.len() {
                    let d = &child_dirs[*dir_idx];
                    *dir_idx += 1;
                    let child_abs = if rel.is_empty() {
                        work_tree.join(&d.name)
                    } else {
                        work_tree.join(rel).join(&d.name)
                    };
                    Some((d.name.clone(), child_abs, true))
                } else if *file_idx < child_files.len() {
                    let n = child_files[*file_idx].clone();
                    *file_idx += 1;
                    // Collapsed directory markers (`dir/`) are already represented in
                    // `ucd.untracked`. Re-traversing them via cache source would treat them as
                    // real directories and duplicate entries across successive status runs.
                    if n.ends_with('/') {
                        continue;
                    }
                    let child_rel = if rel.is_empty() {
                        n.clone()
                    } else {
                        format!("{rel}/{n}")
                    };
                    let child_abs = work_tree.join(&child_rel);
                    let is_dir = child_abs.is_dir();
                    let base = Path::new(&n)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&n)
                        .to_string();
                    Some((base, child_abs, is_dir))
                } else {
                    break;
                }
            }
        };

        let Some((name, path, is_dir)) = next else {
            continue;
        };
        let child_rel = relative_path(rel, &name);

        if is_dir && gitlinks.contains(&child_rel) {
            continue;
        }
        if tracked.contains(&child_rel) {
            continue;
        }

        if is_dir {
            visit_untracked_directory_uc(
                repo,
                index,
                work_tree,
                tracked,
                gitlinks,
                matcher,
                ignored_mode,
                show_all,
                ucd,
                &child_rel,
                &path,
                uc,
            )?;
        } else {
            let (is_ign, _) = matcher.check_path(repo, Some(index), &child_rel, false)?;
            if is_ign {
                continue;
            }
            if use_disk {
                ucd.untracked.push(name);
            }
        }
    }

    if use_disk {
        ucd.dirs.retain(|d| d.recurse);
        ucd.dirs.sort_by(|a, b| a.name.cmp(&b.name));
    }

    let meta = fs::symlink_metadata(abs).map_err(Error::Io)?;
    ucd.stat_data = stat_data_from_meta(&meta);
    if use_disk
        && (rel.is_empty() || !ucd.untracked.is_empty() || ucd.dirs.iter().any(|d| d.recurse))
    {
        ucd.exclude_oid = parent_exclude_oid;
    }
    ucd.valid = true;
    // Match Git's in-memory read_directory behavior: `check_only` directories are kept in
    // the UNTR tree but are not recursively traversed on subsequent status runs.
    if !check_only {
        ucd.recurse = true;
    }

    Ok(())
}

fn visit_untracked_directory_uc(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: UntrackedIgnoredMode,
    show_all: bool,
    parent_ucd: &mut UntrackedCacheDir,
    rel: &str,
    abs: &Path,
    uc: &mut UntrackedCache,
) -> Result<()> {
    let name = Path::new(rel)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(rel)
        .to_string();

    if has_tracked_under(tracked, gitlinks, rel) {
        let child = lookup_or_create_child(parent_ucd, &name, uc);
        return read_directory_recursive(
            repo,
            index,
            work_tree,
            tracked,
            gitlinks,
            matcher,
            ignored_mode,
            show_all,
            false,
            child,
            rel,
            abs,
            uc,
        );
    }

    // Fast prune for default ignored mode: an excluded directory cannot surface untracked
    // entries unless tracked descendants exist (handled above).
    if ignored_mode == UntrackedIgnoredMode::No
        && matcher.check_path(repo, Some(index), rel, true)?.0
    {
        return Ok(());
    }

    if ignored_mode == UntrackedIgnoredMode::Matching
        && show_all
        && matcher.check_path(repo, Some(index), rel, true)?.0
    {
        return Ok(());
    }

    if ignored_mode == UntrackedIgnoredMode::Traditional && !show_all {
        if let Some(line) = traditional_normal_directory_only(
            repo, index, work_tree, tracked, gitlinks, matcher, rel, abs, uc,
        )? {
            let _ = line;
            return Ok(());
        }
    }

    if show_all {
        let child = lookup_or_create_child(parent_ucd, &name, uc);
        return read_directory_recursive(
            repo,
            index,
            work_tree,
            tracked,
            gitlinks,
            matcher,
            ignored_mode,
            true,
            false,
            child,
            rel,
            abs,
            uc,
        );
    }

    if !show_all {
        let reuse_collapsed_index = parent_ucd
            .dirs
            .iter()
            .find(|d| d.name == name && d.check_only)
            .and_then(|target| parent_ucd.dirs.iter().position(|d| std::ptr::eq(d, target)))
            .filter(|&idx| valid_cached_dir(&parent_ucd.dirs[idx], abs, true));
        if let Some(idx) = reuse_collapsed_index {
            let candidate = &parent_ucd.dirs[idx];
            let has_visible =
                check_only_tree_has_visible_untracked(repo, index, matcher, rel, candidate)?;
            parent_ucd.dirs[idx].recurse = true;
            if has_visible {
                let collapsed = format!("{name}/");
                if !parent_ucd.untracked.iter().any(|u| u == &collapsed) {
                    parent_ucd.untracked.push(collapsed);
                }
            }
            return Ok(());
        }
    }

    let mut sub_untracked = Vec::new();
    let mut sub_ignored = Vec::new();
    visit_untracked_node_full(
        repo,
        index,
        work_tree,
        tracked,
        gitlinks,
        matcher,
        ignored_mode,
        true,
        rel,
        abs,
        &mut sub_untracked,
        &mut sub_ignored,
        uc,
    )?;

    if !sub_untracked.is_empty() && !sub_ignored.is_empty() {
        let child = lookup_or_create_child(parent_ucd, &name, uc);
        return read_directory_recursive(
            repo,
            index,
            work_tree,
            tracked,
            gitlinks,
            matcher,
            ignored_mode,
            true,
            false,
            child,
            rel,
            abs,
            uc,
        );
    }

    if sub_untracked.is_empty() && !sub_ignored.is_empty() {
        let has_hidden = has_hidden_untracked_file_or_dir(
            repo, index, tracked, gitlinks, matcher, rel, abs, uc,
        )?;
        if has_hidden {
            let child = lookup_or_create_child(parent_ucd, &name, uc);
            child.recurse = true;
            child.check_only = true;
            child.valid = true;
            child.untracked.clear();
            child.dirs.clear();
            child.exclude_oid = ObjectId::zero();
            if let Ok(meta) = fs::symlink_metadata(abs) {
                child.stat_data = stat_data_from_meta(&meta);
            }
        } else if let Some(child) = parent_ucd
            .dirs
            .iter_mut()
            .find(|d| d.name == name && d.check_only)
        {
            // Keep existing placeholders reusable, but do not create new ones for
            // newly fully-ignored directories (t7063 sparse keep/true cache shape).
            child.recurse = true;
            child.check_only = true;
            child.valid = true;
            child.untracked.clear();
            child.dirs.clear();
            child.exclude_oid = ObjectId::zero();
            if let Ok(meta) = fs::symlink_metadata(abs) {
                child.stat_data = stat_data_from_meta(&meta);
            }
        }
        return Ok(());
    }

    if sub_untracked.is_empty() && sub_ignored.is_empty() {
        if has_ignored_entry_or_dir(repo, index, tracked, gitlinks, matcher, rel, abs)? {
            let child = lookup_or_create_child(parent_ucd, &name, uc);
            child.recurse = true;
            child.check_only = true;
            child.valid = true;
            child.untracked.clear();
            child.dirs.clear();
            child.exclude_oid = ObjectId::zero();
            if let Ok(meta) = fs::symlink_metadata(abs) {
                child.stat_data = stat_data_from_meta(&meta);
            }
            return Ok(());
        }
        if let Some(child) = parent_ucd
            .dirs
            .iter_mut()
            .find(|d| d.name == name && d.check_only)
        {
            // Preserve existing placeholder nodes for directories that now contain no
            // untracked entries but are part of an already-materialized check-only subtree.
            child.recurse = true;
            child.valid = true;
            child.untracked.clear();
            child.dirs.clear();
            child.exclude_oid = ObjectId::zero();
            if let Ok(meta) = fs::symlink_metadata(abs) {
                child.stat_data = stat_data_from_meta(&meta);
            }
        }
        return Ok(());
    }

    if !sub_untracked.is_empty() && sub_ignored.is_empty() {
        // Git `lookup_untracked` allocates a child node even when the visible output collapses
        // the directory to `name/` in normal untracked mode (t7063 dump expectations).
        // Build that child in check-only mode from the already discovered full walk to avoid
        // reopening directories and overcounting `opendir` trace stats.
        let child = lookup_or_create_child(parent_ucd, &name, uc);
        populate_check_only_subtree(child, rel, abs, &sub_untracked, uc);
        let collapsed = format!("{name}/");
        if !parent_ucd.untracked.iter().any(|u| u == &collapsed) {
            parent_ucd.untracked.push(collapsed);
        }
        return Ok(());
    }

    Ok(())
}

fn populate_check_only_subtree(
    root: &mut UntrackedCacheDir,
    rel: &str,
    abs: &Path,
    sub_untracked: &[String],
    uc: &mut UntrackedCache,
) {
    root.untracked.clear();
    root.dirs.clear();
    // Keep check-only directories in UNTR output shape (Git writes them with `recurse` set),
    // but runtime scans skip them via `!d.check_only` in cache traversal.
    root.recurse = true;
    root.check_only = true;
    root.valid = true;
    root.exclude_oid = ObjectId::zero();
    if let Ok(meta) = fs::symlink_metadata(abs) {
        root.stat_data = stat_data_from_meta(&meta);
    }

    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    for full in sub_untracked {
        let rest = if prefix.is_empty() {
            full.as_str()
        } else if let Some(stripped) = full.strip_prefix(&prefix) {
            stripped
        } else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        insert_check_only_path(root, abs, &parts, uc);
    }
    sort_untracked_tree(root);
}

fn insert_check_only_path(
    dir: &mut UntrackedCacheDir,
    dir_abs: &Path,
    parts: &[&str],
    uc: &mut UntrackedCache,
) {
    if parts.is_empty() {
        return;
    }
    if parts.len() == 1 {
        let file = parts[0].to_string();
        if !dir.untracked.iter().any(|u| u == &file) {
            dir.untracked.push(file);
        }
        return;
    }

    let comp = parts[0];
    let collapsed = format!("{comp}/");
    if !dir.untracked.iter().any(|u| u == &collapsed) {
        dir.untracked.push(collapsed);
    }
    let child_abs = dir_abs.join(comp);
    let child = lookup_or_create_child(dir, comp, uc);
    child.recurse = true;
    child.check_only = true;
    child.valid = true;
    child.exclude_oid = ObjectId::zero();
    if let Ok(meta) = fs::symlink_metadata(&child_abs) {
        child.stat_data = stat_data_from_meta(&meta);
    }
    insert_check_only_path(child, &child_abs, &parts[1..], uc);
}

fn sort_untracked_tree(dir: &mut UntrackedCacheDir) {
    dir.untracked.sort();
    dir.untracked.dedup();
    dir.dirs.sort_by(|a, b| a.name.cmp(&b.name));
    for child in &mut dir.dirs {
        sort_untracked_tree(child);
    }
}

fn check_only_tree_has_visible_untracked(
    repo: &Repository,
    index: &Index,
    matcher: &mut IgnoreMatcher,
    rel: &str,
    dir: &UntrackedCacheDir,
) -> Result<bool> {
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };

    for file in &dir.untracked {
        let path = format!("{prefix}{file}");
        let (is_ignored, _) = matcher.check_path(repo, Some(index), &path, false)?;
        if !is_ignored {
            return Ok(true);
        }
    }

    for child in &dir.dirs {
        let child_rel = if rel.is_empty() {
            child.name.clone()
        } else {
            format!("{rel}/{}", child.name)
        };
        if check_only_tree_has_visible_untracked(repo, index, matcher, &child_rel, child)? {
            return Ok(true);
        }
    }

    Ok(false)
}

fn visit_untracked_node_full(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: UntrackedIgnoredMode,
    show_all: bool,
    rel: &str,
    abs: &Path,
    untracked_out: &mut Vec<String>,
    ignored_out: &mut Vec<String>,
    uc: &mut UntrackedCache,
) -> Result<()> {
    let entries = match fs::read_dir(abs) {
        Ok(e) => {
            uc.dir_opened += 1;
            e
        }
        Err(_) => return Ok(()),
    };
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let child_rel = relative_path(rel, &name);
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

        if is_dir && gitlinks.contains(&child_rel) {
            continue;
        }
        if tracked.contains(&child_rel) {
            continue;
        }

        if is_dir {
            visit_untracked_directory_collect(
                repo,
                index,
                work_tree,
                tracked,
                gitlinks,
                matcher,
                ignored_mode,
                show_all,
                &child_rel,
                &path,
                untracked_out,
                ignored_out,
                uc,
            )?;
        } else {
            let (is_ign, _) = matcher.check_path(repo, Some(index), &child_rel, false)?;
            if is_ign {
                if ignored_mode != UntrackedIgnoredMode::No {
                    ignored_out.push(child_rel);
                }
            } else {
                untracked_out.push(child_rel);
            }
        }
    }
    Ok(())
}

fn visit_untracked_directory_collect(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    ignored_mode: UntrackedIgnoredMode,
    show_all: bool,
    rel: &str,
    abs: &Path,
    untracked_out: &mut Vec<String>,
    ignored_out: &mut Vec<String>,
    uc: &mut UntrackedCache,
) -> Result<()> {
    if has_tracked_under(tracked, gitlinks, rel) {
        return visit_untracked_node_full(
            repo,
            index,
            work_tree,
            tracked,
            gitlinks,
            matcher,
            ignored_mode,
            show_all,
            rel,
            abs,
            untracked_out,
            ignored_out,
            uc,
        );
    }

    // Fast prune for default ignored mode: excluded directories cannot contribute visible
    // untracked entries when there are no tracked descendants.
    if ignored_mode == UntrackedIgnoredMode::No
        && matcher.check_path(repo, Some(index), rel, true)?.0
    {
        return Ok(());
    }

    if ignored_mode == UntrackedIgnoredMode::Matching
        && show_all
        && matcher.check_path(repo, Some(index), rel, true)?.0
    {
        ignored_out.push(format!("{rel}/"));
        return Ok(());
    }

    if ignored_mode == UntrackedIgnoredMode::Traditional && !show_all {
        if let Some(line) = traditional_normal_directory_only(
            repo, index, work_tree, tracked, gitlinks, matcher, rel, abs, uc,
        )? {
            ignored_out.push(line);
            return Ok(());
        }
    }

    let mut sub_u = Vec::new();
    let mut sub_i = Vec::new();
    visit_untracked_node_full(
        repo,
        index,
        work_tree,
        tracked,
        gitlinks,
        matcher,
        ignored_mode,
        true,
        rel,
        abs,
        &mut sub_u,
        &mut sub_i,
        uc,
    )?;

    if show_all {
        untracked_out.append(&mut sub_u);
        ignored_out.append(&mut sub_i);
        return Ok(());
    }

    if !sub_u.is_empty() && !sub_i.is_empty() {
        untracked_out.append(&mut sub_u);
        ignored_out.append(&mut sub_i);
        return Ok(());
    }

    if sub_u.is_empty() && !sub_i.is_empty() {
        let dir_excluded = matcher.check_path(repo, Some(index), rel, true)?.0;
        let collapse_matching = ignored_mode == UntrackedIgnoredMode::Matching && dir_excluded;
        let collapse_traditional = ignored_mode == UntrackedIgnoredMode::Traditional;
        if collapse_matching || collapse_traditional {
            ignored_out.push(format!("{rel}/"));
        } else {
            ignored_out.append(&mut sub_i);
        }
        return Ok(());
    }

    if !sub_u.is_empty() && sub_i.is_empty() {
        if rel.is_empty() {
            untracked_out.append(&mut sub_u);
        } else {
            untracked_out.push(format!("{rel}/"));
        }
    }

    Ok(())
}

fn traditional_normal_directory_only(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    rel: &str,
    abs: &Path,
    uc: &mut UntrackedCache,
) -> Result<Option<String>> {
    let mut any_file = false;
    let mut stack = vec![abs.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => {
                uc.dir_opened += 1;
                e
            }
            Err(_) => continue,
        };
        let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        sorted.sort_by_key(|e| e.file_name());
        for entry in sorted {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            let rel_child = if dir == *abs {
                relative_path(rel, &name)
            } else {
                let suffix = path.strip_prefix(work_tree).unwrap_or(&path);
                suffix.to_string_lossy().replace('\\', "/")
            };
            if is_dir && gitlinks.contains(&rel_child) {
                continue;
            }
            if tracked.contains(&rel_child) {
                return Ok(None);
            }
            if is_dir {
                stack.push(path);
            } else {
                any_file = true;
                let (ig, _) = matcher.check_path(repo, Some(index), &rel_child, false)?;
                if !ig {
                    return Ok(None);
                }
            }
        }
    }
    if any_file {
        Ok(Some(format!("{rel}/")))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untracked_extension_round_trip_shell() {
        let uc = UntrackedCache::new_shell(6, b"ident\x00".to_vec());
        let raw = write_untracked_extension(&uc);
        let back = parse_untracked_extension(&raw).expect("parse shell");
        assert_eq!(back.dir_flags, 6);
        assert_eq!(back.ident, uc.ident);
        assert!(back.root.is_none());
    }

    #[test]
    fn untracked_extension_round_trip_with_tree() {
        let mut uc = UntrackedCache::new_shell(6, b"id\x00".to_vec());
        let mut root = UntrackedCacheDir::new(String::new());
        root.valid = true;
        root.recurse = true;
        root.stat_data = StatDataDisk {
            mtime_sec: 1,
            ..Default::default()
        };
        root.untracked = vec!["a".to_string(), "b".to_string()];
        let mut child = UntrackedCacheDir::new("sub".to_string());
        child.valid = true;
        child.recurse = true;
        root.dirs.push(child);
        uc.root = Some(root);

        let raw = write_untracked_extension(&uc);
        let back = parse_untracked_extension(&raw).expect("parse tree");
        assert!(back.root.is_some());
        let r = back.root.as_ref().unwrap();
        assert_eq!(r.untracked.len(), 2);
        assert_eq!(r.dirs.len(), 1);
    }
}

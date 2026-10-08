//! Split index: `link` extension and `sharedindex.<sha1>` (Git `split-index.c`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::ewah_bitmap::EwahBitmap;
use crate::git_date::approx::approxidate_careful;
use crate::index::{Index, IndexEntry};
use crate::objects::{HashAlgo, ObjectId};

/// Split-index metadata carried on an [`Index`] (in-memory; bitmaps cleared after merge/write).
#[derive(Debug, Clone)]
pub(crate) struct SplitIndexLink {
    /// OID of the shared index file (`sharedindex.<hex>`).
    pub base_oid: ObjectId,
    pub delete_bitmap: Option<EwahBitmap>,
    pub replace_bitmap: Option<EwahBitmap>,
}

fn parse_shared_repository_perm(raw: Option<&str>) -> i32 {
    const PERM_UMASK: i32 = 0;
    const OLD_PERM_GROUP: i32 = 1;
    const OLD_PERM_EVERYBODY: i32 = 2;
    const PERM_GROUP: i32 = 0o660;
    const PERM_EVERYBODY: i32 = 0o664;

    let Some(value) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return PERM_UMASK;
    };
    if value.eq_ignore_ascii_case("umask") {
        return PERM_UMASK;
    }
    if value.eq_ignore_ascii_case("group") {
        return PERM_GROUP;
    }
    if value.eq_ignore_ascii_case("all")
        || value.eq_ignore_ascii_case("world")
        || value.eq_ignore_ascii_case("everybody")
    {
        return PERM_EVERYBODY;
    }
    if !value.is_empty() && value.chars().all(|c| ('0'..='7').contains(&c)) {
        if let Ok(i) = i32::from_str_radix(value, 8) {
            return match i {
                PERM_UMASK => PERM_UMASK,
                OLD_PERM_GROUP => PERM_GROUP,
                OLD_PERM_EVERYBODY => PERM_EVERYBODY,
                _ => {
                    if (i & 0o600) != 0o600 {
                        return PERM_UMASK;
                    }
                    -(i & 0o666)
                }
            };
        }
    }
    if value.eq_ignore_ascii_case("true") {
        PERM_GROUP
    } else {
        PERM_UMASK
    }
}

#[cfg(unix)]
fn calc_shared_perm(shared_repo: i32, mode: u32) -> u32 {
    let tweak = if shared_repo < 0 {
        (-shared_repo) as u32
    } else {
        shared_repo as u32
    };

    let mut new_mode = if shared_repo < 0 {
        (mode & !0o777) | tweak
    } else {
        mode | tweak
    };

    if mode & 0o200 == 0 {
        new_mode &= !0o222;
    }
    if mode & 0o100 != 0 {
        new_mode |= (new_mode & 0o444) >> 2;
    }

    new_mode
}

#[cfg(unix)]
fn adjust_shared_perm_file(path: &Path, shared_repo: i32) -> io::Result<()> {
    if shared_repo == 0 {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path)?;
    let old = meta.permissions().mode();
    let new_mode = calc_shared_perm(shared_repo, old);
    if (old ^ new_mode) & 0o777 != 0 {
        let mut p = meta.permissions();
        p.set_mode(new_mode & 0o777);
        fs::set_permissions(path, p)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn adjust_shared_perm_file(_path: &Path, _shared_repo: i32) -> io::Result<()> {
    Ok(())
}

/// Compare on-disk-relevant fields (Git `compare_ce_content` in `split-index.c`).
pub(crate) fn entries_equal_for_split(a: &IndexEntry, b: &IndexEntry) -> bool {
    let mask: u16 = 0xF000 | 0x8000;
    let a_flags = a.flags & mask;
    let b_flags = b.flags & mask;
    let ext_mask: u16 = 0x7000;
    let a_ext = a.flags_extended.unwrap_or(0) & ext_mask;
    let b_ext = b.flags_extended.unwrap_or(0) & ext_mask;
    a.ctime_sec == b.ctime_sec
        && a.ctime_nsec == b.ctime_nsec
        && a.mtime_sec == b.mtime_sec
        && a.mtime_nsec == b.mtime_nsec
        && a.dev == b.dev
        && a.ino == b.ino
        && a.mode == b.mode
        && a.uid == b.uid
        && a.gid == b.gid
        && a.size == b.size
        && a.oid == b.oid
        && a_flags == b_flags
        && a_ext == b_ext
}

fn replace_positions_in_order(link: &SplitIndexLink) -> Vec<usize> {
    let Some(bm) = &link.replace_bitmap else {
        return Vec::new();
    };
    if bm.bit_size == 0 {
        return Vec::new();
    }
    let mut v = Vec::new();
    bm.each_set_bit(|p| v.push(p));
    v
}

fn bitmap_has_bit(bm: &EwahBitmap, i: usize) -> bool {
    let mut found = false;
    bm.each_set_bit(|pos| {
        if pos == i {
            found = true;
        }
    });
    found
}

/// Merge split index + shared base into `index.entries` (Git `merge_base_index`).
pub(crate) fn merge_split_into_index(
    index: &mut Index,
    link: SplitIndexLink,
    base_entries: Vec<IndexEntry>,
) -> Result<()> {
    let saved = std::mem::take(&mut index.entries);
    let replace_pos = replace_positions_in_order(&link);
    let stubs: Vec<IndexEntry> = saved
        .iter()
        .filter(|e| e.path.is_empty())
        .cloned()
        .collect();
    if stubs.len() != replace_pos.len() {
        return Err(Error::IndexError(format!(
            "split index: expected {} replacement stubs, found {}",
            replace_pos.len(),
            stubs.len()
        )));
    }
    let mut stub_iter = stubs.into_iter();
    let rest: Vec<IndexEntry> = saved.into_iter().filter(|e| !e.path.is_empty()).collect();

    let delete = &link.delete_bitmap;
    let replace = &link.replace_bitmap;

    let mut merged: Vec<IndexEntry> = Vec::new();

    for (i, mut base_e) in base_entries.into_iter().enumerate() {
        if delete
            .as_ref()
            .is_some_and(|b| b.bit_size > 0 && bitmap_has_bit(b, i))
        {
            continue;
        }
        if replace
            .as_ref()
            .is_some_and(|b| b.bit_size > 0 && bitmap_has_bit(b, i))
        {
            let Some(rep) = stub_iter.next() else {
                return Err(Error::IndexError(
                    "split index: missing replacement entry".to_owned(),
                ));
            };
            let mut e = rep;
            e.path = base_e.path.clone();
            e.base_index_pos = (i + 1) as u32;
            merged.push(e);
        } else {
            base_e.base_index_pos = (i + 1) as u32;
            merged.push(base_e);
        }
    }

    if stub_iter.next().is_some() {
        return Err(Error::IndexError(
            "split index: too many replacement stubs".to_owned(),
        ));
    }

    for mut e in rest {
        e.base_index_pos = 0;
        merged.push(e);
    }

    merged.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
    index.entries = merged;
    Ok(())
}

/// Parse the `link` extension payload (Git `read_link_extension`).
pub(crate) fn parse_link_extension(data: &[u8], algo: HashAlgo) -> Result<SplitIndexLink> {
    let hash_len = algo.len();
    if data.len() < hash_len {
        return Err(Error::IndexError(
            "corrupt link extension (too short)".to_owned(),
        ));
    }
    let base_oid = ObjectId::from_bytes(&data[..hash_len])?;
    let mut rest = &data[hash_len..];
    if rest.is_empty() {
        return Ok(SplitIndexLink {
            base_oid,
            delete_bitmap: None,
            replace_bitmap: None,
        });
    }
    let Some((del, consumed)) = EwahBitmap::deserialize_prefix(rest) else {
        return Err(Error::IndexError(
            "corrupt delete bitmap in link extension".to_owned(),
        ));
    };
    rest = &rest[consumed..];
    let Some((rep, consumed2)) = EwahBitmap::deserialize_prefix(rest) else {
        return Err(Error::IndexError(
            "corrupt replace bitmap in link extension".to_owned(),
        ));
    };
    rest = &rest[consumed2..];
    if !rest.is_empty() {
        return Err(Error::IndexError(
            "garbage at the end of link extension".to_owned(),
        ));
    }
    Ok(SplitIndexLink {
        base_oid,
        delete_bitmap: Some(del),
        replace_bitmap: Some(rep),
    })
}

/// Serialize `link` extension: base OID plus two EWAH bitmaps (Git always writes both after `prepare_to_write_split_index`).
pub(crate) fn serialize_link_extension_payload(
    base_oid: &ObjectId,
    delete: &EwahBitmap,
    replace: &EwahBitmap,
) -> Vec<u8> {
    let mut out = base_oid.as_bytes().to_vec();
    delete.serialize(&mut out);
    replace.serialize(&mut out);
    out
}

/// Resolve path to shared index file (Git `read_index_from`), with fallbacks when `git_dir` does
/// not match the repo that owns the index (nested trash repo + `GIT_INDEX_FILE`).
fn resolve_shared_index_file(
    git_dir: &Path,
    index_path: &Path,
    base_oid: &ObjectId,
    cwd: &Path,
) -> PathBuf {
    let name = format!("sharedindex.{}", base_oid.to_hex());
    let primary = git_dir.join(&name);

    let try_path = |p: PathBuf| -> Option<PathBuf> {
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    };

    if let Some(p) = try_path(primary.clone()) {
        return p;
    }
    if let Some(parent) = index_path.parent() {
        if let Some(p) = try_path(parent.join(&name)) {
            return p;
        }
    }
    {
        let mut dir = cwd;
        loop {
            if let Some(p) = try_path(dir.join(".git").join(&name)) {
                return p;
            }
            let Some(p) = dir.parent() else {
                break;
            };
            dir = p;
        }
    }
    if let Some(d) = index_path.parent() {
        if let Ok(read) = fs::read_dir(d) {
            for ent in read.flatten() {
                let Ok(ft) = ent.file_type() else {
                    continue;
                };
                if !ft.is_dir() {
                    continue;
                }
                if let Some(p) = try_path(ent.path().join(".git").join(&name)) {
                    return p;
                }
            }
        }
    }
    primary
}

pub(crate) fn hash_index_body(body: &[u8], algo: HashAlgo) -> ObjectId {
    algo.digest(body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitIndexConfig {
    Disabled,
    Unset,
    Enabled,
}

pub(crate) fn split_index_config(cfg: &ConfigSet) -> SplitIndexConfig {
    match cfg.get("core.splitIndex") {
        None => SplitIndexConfig::Unset,
        Some(v) => {
            let t = v.trim();
            if t.eq_ignore_ascii_case("false") || t == "0" {
                SplitIndexConfig::Disabled
            } else if t.eq_ignore_ascii_case("true") || t == "1" {
                SplitIndexConfig::Enabled
            } else {
                SplitIndexConfig::Unset
            }
        }
    }
}

pub(crate) fn max_percent_split_change(cfg: &ConfigSet) -> i32 {
    match cfg.get("splitIndex.maxPercentChange") {
        None => -1,
        Some(v) => v.trim().parse::<i32>().unwrap_or(-1),
    }
}

fn default_max_percent() -> i32 {
    20
}

pub(crate) fn should_rebuild_shared_index(index: &Index, cfg: &ConfigSet) -> bool {
    let max_split = max_percent_split_change(cfg);
    let max_split = match max_split {
        -1 => default_max_percent(),
        0 => return true,
        100 => return false,
        n => n,
    };
    let mut not_shared = 0u64;
    for e in &index.entries {
        if e.base_index_pos == 0 {
            not_shared += 1;
        }
    }
    let total = index.entries.len() as u64;
    if total == 0 {
        return false;
    }
    total * (max_split as u64) < not_shared * 100
}

pub(crate) fn git_test_split_index_force_reorder(
    git_test_split_index: bool,
    base_oid: &ObjectId,
) -> bool {
    git_test_split_index && (base_oid.as_bytes()[0] & 15) < 6
}

pub(crate) fn shared_index_expire_threshold(cfg: &ConfigSet) -> u64 {
    let raw = cfg
        .get("splitIndex.sharedIndexExpire")
        .map(|s| s.trim().to_owned());
    let spec = raw
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("2.weeks.ago");
    if spec.eq_ignore_ascii_case("never") {
        return 0;
    }
    let mut err = 0;
    approxidate_careful(spec, Some(&mut err))
}

fn should_delete_shared_index(path: &Path, expiration: u64) -> bool {
    if expiration == 0 {
        return false;
    }
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mtime() as u64 <= expiration
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

pub(crate) fn clean_stale_shared_index_files(git_dir: &Path, current_hex: &str, cfg: &ConfigSet) {
    let expiration = shared_index_expire_threshold(cfg);
    let Ok(read_dir) = fs::read_dir(git_dir) else {
        return;
    };
    for ent in read_dir.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(hex) = name.strip_prefix("sharedindex.") else {
            continue;
        };
        if hex == current_hex {
            continue;
        }
        let path = ent.path();
        if should_delete_shared_index(&path, expiration) {
            let _ = fs::remove_file(&path);
        }
    }
}

pub(crate) fn freshen_shared_index(path: &Path) {
    let _ = filetime_set_to_now(path);
}

#[cfg(unix)]
fn filetime_set_to_now(path: &Path) -> io::Result<()> {
    use std::time::SystemTime;
    let t = SystemTime::now();
    let ft = filetime::FileTime::from_system_time(t);
    filetime::set_file_mtime(path, ft)
}

#[cfg(not(unix))]
fn filetime_set_to_now(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Request from `update-index` for the next index write.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteSplitIndexRequest {
    /// `Some(true)` / `Some(false)` for `--[no-]split-index`; `None` uses config / test env only.
    pub explicit: Option<bool>,
}

impl WriteSplitIndexRequest {
    /// Whether the next write should use split-index format.
    ///
    /// Matches Git: `--split-index` still enables split index when `core.splitIndex` is false,
    /// but emits a warning (see `builtin/update-index.c`).
    ///
    /// When `explicit` is `None`, an index that was already split (`split_link` set after load)
    /// stays split until `--no-split-index` (Git keeps `istate->split_index` across commands).
    pub fn want_write_split(
        self,
        cfg: &ConfigSet,
        index: &Index,
        git_test_split_index: bool,
    ) -> bool {
        match self.explicit {
            Some(false) => {
                if matches!(split_index_config(cfg), SplitIndexConfig::Enabled) {
                    eprintln!(
                        "warning: core.splitIndex is set to true; remove or change it, if you really want to disable split index"
                    );
                }
                false
            }
            Some(true) => {
                if matches!(split_index_config(cfg), SplitIndexConfig::Disabled) {
                    eprintln!(
                        "warning: core.splitIndex is set to false; remove or change it, if you really want to enable split index"
                    );
                }
                true
            }
            None => {
                if matches!(split_index_config(cfg), SplitIndexConfig::Disabled) {
                    return false;
                }
                index.split_link.is_some()
                    || matches!(split_index_config(cfg), SplitIndexConfig::Enabled)
                    || git_test_split_index
            }
        }
    }
}

fn find_entry_pos_sorted(entries: &[IndexEntry], path: &[u8], stage: u8) -> Option<usize> {
    entries
        .binary_search_by(|e| {
            e.path
                .as_slice()
                .cmp(path)
                .then_with(|| e.stage().cmp(&stage))
        })
        .ok()
}

fn load_shared_entries(
    git_dir: &Path,
    index_path: &Path,
    base_oid: &ObjectId,
    cwd: &Path,
) -> Result<Vec<IndexEntry>> {
    let p = resolve_shared_index_file(git_dir, index_path, base_oid, cwd);
    let data = fs::read(&p).map_err(Error::Io)?;
    let mut shared = Index::parse(&data)?;
    for (i, e) in shared.entries.iter_mut().enumerate() {
        e.base_index_pos = (i + 1) as u32;
    }
    Ok(shared.entries)
}

/// Write split index to `path` under `git_dir`, updating `index` base positions and `split_link`.
pub(crate) fn write_index_file_split(
    path: &Path,
    git_dir: &Path,
    index: &mut Index,
    cfg: &ConfigSet,
    request: WriteSplitIndexRequest,
    skip_hash: bool,
    verify_cache_tree: bool,
    env: &crate::environment::Environment,
) -> Result<()> {
    // Mirror upstream `write_locked_index`: under GIT_TEST_CHECK_CACHE_TREE, verify the cache-tree
    // against the index before persisting. A duplicate-entry tree (t4058) produces a cache-tree
    // whose entry counts exceed the deduplicated index, which must abort the write with the
    // canonical "corrupted cache-tree" error rather than silently writing a broken index.
    if verify_cache_tree {
        crate::write_tree::verify_cache_tree(index)?;
    }

    let git_test_split_index = env.git_test_split_index;
    let want_split = request.want_write_split(cfg, index, git_test_split_index);

    let shared_repo = parse_shared_repository_perm(cfg.get("core.sharedRepository").as_deref());

    if !want_split {
        index.split_link = None;
        for e in &mut index.entries {
            e.base_index_pos = 0;
        }
        index.write_to_path_with_config(path, skip_hash, Some(cfg))?;
        adjust_shared_perm_file(path, shared_repo).map_err(Error::Io)?;
        return Ok(());
    }

    // Git `alternate_index_output`: split index is only written to the repository's primary index
    // file (`$GIT_DIR/index`). `GIT_INDEX_FILE` pointing elsewhere gets a unified index (t1700 #25).
    let default_index = git_dir.join("index");
    let is_primary_index = path == default_index
        || path
            .canonicalize()
            .ok()
            .zip(default_index.canonicalize().ok())
            .is_some_and(|(a, b)| a == b);
    if !is_primary_index {
        index.split_link = None;
        for e in &mut index.entries {
            e.base_index_pos = 0;
        }
        index.write_to_path_with_config(path, skip_hash, Some(cfg))?;
        adjust_shared_perm_file(path, shared_repo).map_err(Error::Io)?;
        return Ok(());
    }

    if index.sparse_directories {
        return Err(Error::IndexError(
            "cannot write split index for a sparse index".to_owned(),
        ));
    }

    let prev_base = index
        .split_link
        .as_ref()
        .map(|l| l.base_oid)
        .unwrap_or(ObjectId::zero());

    let mut rebuild = index.split_link.is_none()
        || should_rebuild_shared_index(index, cfg)
        || git_test_split_index_force_reorder(git_test_split_index, &prev_base);

    if git_test_split_index && index.split_link.is_none() {
        rebuild = true;
    }

    let base_snapshot: Vec<IndexEntry> = if rebuild {
        let mut v: Vec<IndexEntry> = index.entries.to_vec();
        v.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
        for (i, e) in v.iter_mut().enumerate() {
            e.base_index_pos = (i + 1) as u32;
        }
        v
    } else {
        let link = index.split_link.as_ref().ok_or_else(|| {
            Error::IndexError("split index missing base link during reuse".to_owned())
        })?;
        load_shared_entries(git_dir, path, &link.base_oid, &env.cwd)?
    };

    // After a shared-index rebuild, `base_snapshot` matches the merged index exactly; align indices
    // (e.g. `--no-split-index` then `--split-index`). When reusing an on-disk shared file, do not
    // remap by path — deleted paths can still exist in the shared index until expiry/rebuild, and
    // re-adding the same path must stay split-only (`base_index_pos` 0) like Git.
    if rebuild {
        for e in &mut index.entries {
            if let Some(i) = base_snapshot
                .iter()
                .position(|b| b.path == e.path && b.stage() == e.stage())
            {
                e.base_index_pos = (i + 1) as u32;
            } else {
                e.base_index_pos = 0;
            }
        }
    }

    let base_oid = if rebuild {
        let shared_index = Index {
            version: index.version,
            entries: base_snapshot.clone(),
            entries_sorted: true,
            sparse_directories: false,
            untracked_cache: None,
            fsmonitor_last_update: None,
            fsmonitor_dirty: None,
            fsmonitor_raw_extension: None,
            resolve_undo: None,
            split_link: None,
            cache_tree_root: None,
            cache_tree: None,
            hash_algo: index.hash_algo,
            source_mtime: None,
        };
        let tmp = match tempfile::NamedTempFile::new_in(git_dir) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                // Git: mks_tempfile_sm failure falls back to a unified index (no `link` extension).
                index.split_link = None;
                for e in &mut index.entries {
                    e.base_index_pos = 0;
                }
                index.write_to_path_with_config(path, skip_hash, Some(cfg))?;
                adjust_shared_perm_file(path, shared_repo).map_err(Error::Io)?;
                return Ok(());
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let tmp_path = tmp.path().to_path_buf();
        shared_index.write_to_path_with_config(&tmp_path, skip_hash, Some(cfg))?;
        adjust_shared_perm_file(&tmp_path, shared_repo).map_err(Error::Io)?;
        let file_data = fs::read(&tmp_path).map_err(Error::Io)?;
        let hash_len = index.hash_algo.len();
        if file_data.len() < hash_len {
            return Err(Error::IndexError("shared index temp too short".to_owned()));
        }
        let body = &file_data[..file_data.len() - hash_len];
        let oid = hash_index_body(body, index.hash_algo);
        let dest = git_dir.join(format!("sharedindex.{}", oid.to_hex()));
        if let Err(e) = fs::rename(&tmp_path, &dest) {
            if e.kind() == io::ErrorKind::PermissionDenied {
                let _ = fs::remove_file(&tmp_path);
                index.split_link = None;
                for ent in &mut index.entries {
                    ent.base_index_pos = 0;
                }
                index.write_to_path_with_config(path, skip_hash, Some(cfg))?;
                adjust_shared_perm_file(path, shared_repo).map_err(Error::Io)?;
                return Ok(());
            }
            return Err(Error::Io(e));
        }
        clean_stale_shared_index_files(git_dir, &oid.to_hex(), cfg);
        oid
    } else {
        let oid = index
            .split_link
            .as_ref()
            .ok_or_else(|| {
                Error::IndexError("split index missing base link during reuse".to_owned())
            })?
            .base_oid;
        freshen_shared_index(&resolve_shared_index_file(git_dir, path, &oid, &env.cwd));
        oid
    };

    // Map each shared-index row to the merged entry that claims it (`ce->index`), like Git
    // `prepare_to_write_split_index` (path must still match that row).
    let mut merged_by_pos: Vec<Option<usize>> = vec![None; base_snapshot.len()];
    for (p, e) in index.entries.iter().enumerate() {
        if e.base_index_pos == 0 {
            continue;
        }
        let i = e.base_index_pos.saturating_sub(1) as usize;
        if i < base_snapshot.len()
            && base_snapshot[i].path == e.path
            && base_snapshot[i].stage() == e.stage()
        {
            merged_by_pos[i] = Some(p);
        }
    }

    let mut delete_bm = EwahBitmap::new();
    let mut replace_bm = EwahBitmap::new();
    let mut main_entries: Vec<IndexEntry> = Vec::new();

    for i in 0..base_snapshot.len() {
        let b = &base_snapshot[i];
        if let Some(p) = merged_by_pos[i] {
            let c = &index.entries[p];
            if entries_equal_for_split(b, c) {
                continue;
            }
            replace_bm.set_bit_extend(i);
            let mut stub = c.clone();
            stub.path.clear();
            stub.base_index_pos = 0;
            main_entries.push(stub);
        } else {
            delete_bm.set_bit_extend(i);
        }
    }

    for e in &index.entries {
        if e.base_index_pos == 0 {
            let mut c = e.clone();
            c.base_index_pos = 0;
            main_entries.push(c);
            continue;
        }
        let i = e.base_index_pos.saturating_sub(1) as usize;
        if i >= base_snapshot.len()
            || base_snapshot[i].path != e.path
            || base_snapshot[i].stage() != e.stage()
        {
            let mut c = e.clone();
            c.base_index_pos = 0;
            main_entries.push(c);
            continue;
        }
        if entries_equal_for_split(&base_snapshot[i], e) {
            continue;
        }
        // Replacement: stub already pushed above.
    }

    main_entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));

    let link = SplitIndexLink {
        base_oid,
        delete_bitmap: Some(delete_bm),
        replace_bitmap: Some(replace_bm),
    };

    let out_index = Index {
        version: index.version,
        entries: main_entries,
        entries_sorted: true,
        sparse_directories: false,
        untracked_cache: index.untracked_cache.clone(),
        fsmonitor_last_update: index.fsmonitor_last_update.clone(),
        fsmonitor_dirty: index.fsmonitor_dirty.clone(),
        fsmonitor_raw_extension: index.fsmonitor_raw_extension.clone(),
        resolve_undo: None,
        split_link: Some(link),
        cache_tree_root: index.cache_tree_root,
        cache_tree: index.cache_tree.clone(),
        hash_algo: index.hash_algo,
        source_mtime: index.source_mtime,
    };

    out_index.write_to_path_with_config(path, skip_hash, Some(cfg))?;
    adjust_shared_perm_file(path, shared_repo).map_err(Error::Io)?;

    for e in &mut index.entries {
        if let Some(pos) = find_entry_pos_sorted(&base_snapshot, &e.path, e.stage()) {
            if entries_equal_for_split(&base_snapshot[pos], e) {
                e.base_index_pos = (pos + 1) as u32;
                continue;
            }
        }
        e.base_index_pos = 0;
    }

    index.split_link = Some(SplitIndexLink {
        base_oid,
        delete_bitmap: None,
        replace_bitmap: None,
    });

    Ok(())
}

/// Human-readable split-index dump for `test-tool dump-split-index`.
/// If `index` has a split `link` extension, load the shared index and merge entries.
pub fn resolve_split_index_if_needed(
    index: &mut Index,
    git_dir: &Path,
    index_path: &Path,
    cwd: &Path,
) -> Result<()> {
    let Some(link) = index.split_link.clone() else {
        return Ok(());
    };
    if link.base_oid.is_zero() {
        return Ok(());
    }
    let base_oid = link.base_oid;
    let shared_path = resolve_shared_index_file(git_dir, index_path, &base_oid, cwd);
    let data = fs::read(&shared_path).map_err(|e| {
        Error::IndexError(format!(
            "split index: cannot read shared index {}: {e}",
            shared_path.display()
        ))
    })?;
    let hash_len = index.hash_algo.len();
    if data.len() < hash_len {
        return Err(Error::IndexError(
            "split index: shared index too short".to_owned(),
        ));
    }
    let body = &data[..data.len() - hash_len];
    let got = hash_index_body(body, index.hash_algo);
    if got != base_oid {
        return Err(Error::IndexError(format!(
            "broken index, expect {} in {}, got {}",
            base_oid.to_hex(),
            shared_path.display(),
            got.to_hex()
        )));
    }
    freshen_shared_index(&shared_path);
    let base_entries = Index::parse(&data)?.entries;
    merge_split_into_index(index, link, base_entries)?;
    index.split_link = Some(SplitIndexLink {
        base_oid,
        delete_bitmap: None,
        replace_bitmap: None,
    });
    Ok(())
}

/// Format output for `test-tool dump-split-index` (Git reads the index with `do_read_index` only,
/// without merging the shared base — stubs and EWAH bitmaps stay intact).
pub fn format_dump_split_index_file(data: &[u8], index: &Index) -> Result<String> {
    use std::fmt::Write;
    let hash_len = index.hash_algo.len();
    if data.len() < hash_len {
        return Err(Error::IndexError("index too short".to_owned()));
    }
    let body = &data[..data.len() - hash_len];
    let trail = &data[data.len() - hash_len..];
    let own = if trail.iter().all(|&b| b == 0) {
        hash_index_body(body, index.hash_algo)
    } else {
        ObjectId::from_bytes(trail)?
    };

    let mut s = String::new();
    writeln!(s, "own {}", own.to_hex()).map_err(|e| Error::IndexError(e.to_string()))?;
    let Some(link) = &index.split_link else {
        writeln!(s, "not a split index").map_err(|e| Error::IndexError(e.to_string()))?;
        return Ok(s);
    };
    writeln!(s, "base {}", link.base_oid.to_hex()).map_err(|e| Error::IndexError(e.to_string()))?;
    for e in &index.entries {
        // Split-index replacement stubs use `CE_STRIP_NAME`: zero-length path on disk (Git still prints the line).
        let path_disp = String::from_utf8_lossy(&e.path);
        writeln!(
            s,
            "{:06o} {} {}\t{}",
            e.mode,
            e.oid.to_hex(),
            e.stage(),
            path_disp
        )
        .map_err(|e| Error::IndexError(e.to_string()))?;
    }
    write!(s, "replacements:").map_err(|e| Error::IndexError(e.to_string()))?;
    if let Some(bm) = &link.replace_bitmap {
        bm.each_set_bit(|pos| {
            write!(s, " {}", pos).ok();
        });
    }
    writeln!(s).map_err(|e| Error::IndexError(e.to_string()))?;
    write!(s, "deletions:").map_err(|e| Error::IndexError(e.to_string()))?;
    if let Some(bm) = &link.delete_bitmap {
        bm.each_set_bit(|pos| {
            write!(s, " {}", pos).ok();
        });
    }
    writeln!(s).map_err(|e| Error::IndexError(e.to_string()))?;
    Ok(s)
}

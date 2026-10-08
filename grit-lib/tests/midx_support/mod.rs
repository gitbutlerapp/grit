//! Shared helpers for multi-pack-index scenario tests (t5319 / t5334 / t5335).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use grit_lib::midx::{
    midx_lookup_pack_and_offset, read_midx_objects, try_read_info_via_midx,
    try_read_object_via_midx, write_multi_pack_index_with_options, WriteMultiPackIndexOptions,
};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::{clear_pack_cache, read_idx_object_ids, read_pack_index_cached};
use grit_test_support::objects::{
    git_cat_file_batch_check, git_supports_sha256, BatchCheckOutcome, HashAlgo,
};

pub use grit_test_support::objects::RepoFixture;

/// Write a new pack containing every reachable object (keeps existing packs).
pub fn pack_all_objects(repo: &RepoFixture, basename: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let rev = match Command::new("git")
        .current_dir(repo.path())
        .args(["rev-list", "--objects", "--all"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let out_path = format!(".git/objects/pack/{basename}");
    let mut child = match Command::new("git")
        .current_dir(repo.path())
        .args(["pack-objects", &out_path])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(stdin) = child.stdin.as_mut() {
        if stdin.write_all(&rev.stdout).is_err() {
            return false;
        }
    }
    child.wait().ok().is_some_and(|s| s.success())
}

/// Repo with one commit whose HEAD oid appears in at least two `.idx` files.
pub fn duplicate_oid_two_pack_repo() -> Option<(RepoFixture, PathBuf, ObjectId)> {
    let repo = RepoFixture::init(HashAlgo::Sha1).ok()?;
    configure_repo_no_gc(&repo);
    repo.git(&["checkout", "-b", "main"]);
    std::fs::write(repo.path().join("dup.txt"), b"duplicate oid fixture\n").ok()?;
    repo.git(&["add", "dup.txt"]);
    repo.git(&["commit", "-q", "-m", "dup seed"]);
    let oid = head_oid(&repo);
    if !pack_all_objects(&repo, "dup-a") || !pack_all_objects(&repo, "dup-b") {
        return None;
    }
    let objects = repo.objects_dir();
    let mut idx_hits = 0usize;
    for idx in pack_idx_paths(&objects) {
        if read_idx_object_ids(&idx)
            .ok()
            .is_some_and(|ids| ids.contains(&oid))
        {
            idx_hits += 1;
        }
    }
    if idx_hits < 2 {
        return None;
    }
    Some((repo, objects, oid))
}

fn append_pack_blob(body: &mut Vec<u8>, data: &[u8]) {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write as _;
    let compressed = {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).expect("zlib");
        enc.finish().expect("zlib finish")
    };
    let type_code = 3u8;
    let mut size = data.len();
    let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
    size >>= 4;
    if size > 0 {
        body.push(first | 0x80);
        while size > 0 {
            let b = (size & 0x7f) as u8;
            size >>= 7;
            body.push(if size > 0 { b | 0x80 } else { b });
        }
    } else {
        body.push(first);
    }
    body.extend_from_slice(&compressed);
}

/// Sparse pack with two blobs (Git requires ≥2 objects for a 64-bit idx extension slot).
/// The large blob lives at offset `1 << 32`; grit writes a v2 `.idx` Git accepts.
pub fn install_git_large_offset_pack(repo: &RepoFixture) -> Option<ObjectId> {
    use grit_lib::objects::ObjectKind;
    use grit_lib::odb::Odb;
    use grit_lib::pack::{skip_one_pack_object, write_v2_pack_index};
    use std::io::{Read, Seek, SeekFrom, Write};

    let objects = repo.objects_dir();
    let odb = Odb::new(&objects);
    let small = b"lo";
    let large_data = b"large-loff-midx-fixture";
    let oid_small = odb.hash(ObjectKind::Blob, small);
    let oid_large = odb.hash(ObjectKind::Blob, large_data);
    let pack_path = objects.join("pack/large-loff.pack");
    let idx_path = pack_path.with_extension("idx");
    let large_offset: u64 = 1 << 32;
    let small_offset: u64 = 12;

    let mut small_obj = Vec::new();
    append_pack_blob(&mut small_obj, small);
    let mut large_obj = Vec::new();
    append_pack_blob(&mut large_obj, large_data);

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(&pack_path)
        .ok()?;
    file.write_all(b"PACK").ok()?;
    file.write_all(&2u32.to_be_bytes()).ok()?;
    file.write_all(&2u32.to_be_bytes()).ok()?;
    file.write_all(&small_obj).ok()?;
    file.seek(SeekFrom::Start(large_offset)).ok()?;
    file.write_all(&large_obj).ok()?;
    let body_end = file.stream_position().ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut hasher = grit_lib::objects::HashAlgo::Sha1.hasher();
    let mut buf = [0u8; 64 * 1024];
    let mut left = body_end;
    while left > 0 {
        let chunk = left.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..chunk]).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    let trailer = hasher.finalize();
    file.seek(SeekFrom::Start(body_end)).ok()?;
    file.write_all(trailer.as_bytes()).ok()?;
    file.flush().ok()?;
    drop(file);

    let pack_bytes = std::fs::read(&pack_path).ok()?;
    let mut end = small_offset as usize;
    skip_one_pack_object(&pack_bytes, &mut end, small_offset, 20).ok()?;
    let crc_small = crc32fast::hash(&pack_bytes[small_offset as usize..end]);
    let mut end_large = large_offset as usize;
    skip_one_pack_object(&pack_bytes, &mut end_large, large_offset, 20).ok()?;
    let crc_large = crc32fast::hash(&pack_bytes[large_offset as usize..end_large]);
    write_v2_pack_index(
        &idx_path,
        &pack_path,
        &[
            (oid_small, small_offset, crc_small),
            (oid_large, large_offset, crc_large),
        ],
        20,
    )
    .ok()?;
    let idx_len = std::fs::metadata(&idx_path).ok()?.len();
    // Git v2 idx with one 64-bit extension slot (2 objects, 1 large offset) is 1136 bytes.
    if idx_len != 1136 {
        return None;
    }
    read_idx_object_ids(&idx_path)
        .ok()
        .filter(|ids| ids.contains(&oid_large))
        .map(|_| oid_large)
}

pub fn pack_objects_layer(dir: &Path, layer: usize) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let rev = match Command::new("git")
        .current_dir(dir)
        .args(["rev-list", "--objects", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let mut child = match Command::new("git")
        .current_dir(dir)
        .args(["pack-objects", &format!(".git/objects/pack/layer-{layer}")])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(stdin) = child.stdin.as_mut() {
        if stdin.write_all(&rev.stdout).is_err() {
            return false;
        }
    }
    child.wait().ok().is_some_and(|s| s.success())
}

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn configure_repo_no_gc(repo: &RepoFixture) {
    repo.git(&["config", "gc.auto", "0"]);
    repo.git(&["config", "maintenance.auto", "false"]);
}

pub fn git_cat_file_batch(
    repo_or_objects: &Path,
    oids: &[ObjectId],
) -> Vec<(ObjectId, ObjectKind, Vec<u8>)> {
    let (git_dir, work_tree): (PathBuf, Option<PathBuf>) = if repo_or_objects.ends_with("objects") {
        let git_dir = repo_or_objects.parent().expect("git dir").to_path_buf();
        let work_tree = git_dir.parent().map(Path::to_path_buf);
        (git_dir, work_tree)
    } else {
        (
            repo_or_objects.join(".git"),
            Some(repo_or_objects.to_path_buf()),
        )
    };
    let mut cmd = Command::new("git");
    cmd.args(["cat-file", "--batch"])
        .env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(wt) = work_tree {
        cmd.env("GIT_WORK_TREE", wt);
    }
    let mut child = cmd.spawn().expect("spawn cat-file");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for oid in oids {
            writeln!(stdin, "{}", oid.to_hex()).expect("write oid");
        }
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait cat-file");
    assert!(
        out.status.success(),
        "git cat-file --batch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut reader = BufReader::new(out.stdout.as_slice());
    let mut parsed = Vec::with_capacity(oids.len());
    for oid in oids {
        let mut hdr = String::new();
        reader.read_line(&mut hdr).expect("header");
        let mut parts = hdr.split_whitespace();
        let got_oid = ObjectId::from_hex(parts.next().expect("oid")).expect("hex");
        assert_eq!(got_oid, *oid);
        let kind = match parts.next().expect("kind") {
            "blob" => ObjectKind::Blob,
            "tree" => ObjectKind::Tree,
            "commit" => ObjectKind::Commit,
            "tag" => ObjectKind::Tag,
            other => panic!("unexpected kind {other}"),
        };
        let size: usize = parts.next().expect("size").parse().expect("size");
        let mut data = vec![0u8; size + 1];
        reader.read_exact(&mut data).expect("payload");
        assert_eq!(data[size], b'\n');
        data.truncate(size);
        parsed.push((got_oid, kind, data));
    }
    parsed
}

pub fn pack_idx_paths(objects: &Path) -> Vec<PathBuf> {
    let pack_dir = objects.join("pack");
    let mut out = Vec::new();
    for ent in std::fs::read_dir(&pack_dir).expect("read pack dir") {
        let ent = ent.expect("dirent");
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.ends_with(".idx") {
            out.push(ent.path());
        }
    }
    out.sort();
    out
}

pub fn all_packed_oids(objects: &Path) -> HashSet<ObjectId> {
    let mut oids = HashSet::new();
    for idx in pack_idx_paths(objects) {
        for oid in read_idx_object_ids(&idx).expect("read idx oids") {
            oids.insert(oid);
        }
    }
    oids
}

pub fn head_oid(repo: &RepoFixture) -> ObjectId {
    let out = repo.git(&["rev-parse", "HEAD"]);
    assert!(out.ok, "rev-parse HEAD: {}", out.stderr);
    ObjectId::from_hex(out.stdout.trim()).expect("parse HEAD")
}

/// Build a repo with `layer_count` separate packs (one commit + repack -d per layer).
pub fn multi_pack_repo(
    algo: HashAlgo,
    layer_count: usize,
) -> Option<(RepoFixture, PathBuf, Vec<ObjectId>)> {
    if !git_available() {
        return None;
    }
    if matches!(algo, HashAlgo::Sha256) && !git_supports_sha256() {
        return None;
    }
    let repo = RepoFixture::init(match algo {
        HashAlgo::Sha1 => HashAlgo::Sha1,
        HashAlgo::Sha256 => HashAlgo::Sha256,
    })
    .ok()?;
    configure_repo_no_gc(&repo);
    repo.git(&["checkout", "-b", "main"]);
    let mut commit_oids = Vec::new();
    for i in 0..layer_count {
        std::fs::write(
            repo.path().join(format!("layer-{i}.txt")),
            format!("pack layer {i}\n"),
        )
        .ok()?;
        let add = repo.git(&["add", &format!("layer-{i}.txt")]);
        if !add.ok {
            return None;
        }
        let c = repo.git(&["commit", "-q", "-m", &format!("c{i}")]);
        if !c.ok {
            return None;
        }
        commit_oids.push(head_oid(&repo));
        if !pack_objects_layer(repo.path(), i) {
            return None;
        }
    }
    let objects = repo.objects_dir();
    if pack_idx_paths(&objects).len() < layer_count {
        return None;
    }
    Some((repo, objects, commit_oids))
}

pub fn git_write_midx(repo: &RepoFixture) {
    let out = repo.git(&["multi-pack-index", "write"]);
    assert!(out.ok, "git midx write: {}", out.stderr);
}

pub fn git_write_midx_incremental(repo: &RepoFixture) -> bool {
    repo.git(&["multi-pack-index", "write", "--incremental"]).ok
}

pub fn assert_git_midx_verify(repo: &RepoFixture) {
    let v = repo.git(&["multi-pack-index", "verify"]);
    assert!(v.ok, "git multi-pack-index verify: {}", v.stderr);
}

/// Write a MIDX with grit, defaulting to on-disk v1 when `version` is unset so system
/// `git multi-pack-index verify` accepts the file on older Git builds.
pub fn grit_write_midx(pack_dir: &Path, opts: &WriteMultiPackIndexOptions) {
    let mut opts = opts.clone();
    if opts.version.is_none() {
        opts.version = Some(1);
    }
    write_multi_pack_index_with_options(pack_dir, &opts).expect("grit write MIDX");
    clear_pack_cache();
}

pub fn assert_grit_midx_reads_match_git(objects: &Path, oids: &[ObjectId]) {
    let git_objects = git_cat_file_batch(objects, oids);
    for (oid, kind, data) in git_objects {
        let via_midx = try_read_object_via_midx(objects, &oid)
            .expect("midx read")
            .expect("object listed in MIDX");
        assert_eq!(via_midx.kind, kind);
        assert_eq!(via_midx.data, data);
        let info = try_read_info_via_midx(objects, &oid)
            .expect("midx info")
            .expect("info for listed oid");
        assert_eq!(info.kind, kind);
        assert_eq!(info.size, u64::try_from(data.len()).expect("size fits u64"));
    }
}

pub fn assert_lookup_matches_idx(objects: &Path, oid: &ObjectId) {
    let (pack_id, off) = midx_lookup_pack_and_offset(objects, oid).expect("midx lookup");
    let pack_dir = objects.join("pack");
    let idx_name = read_midx_objects(objects)
        .expect("read midx objects")
        .0
        .get(pack_id as usize)
        .expect("pack id")
        .clone();
    let idx = read_pack_index_cached(&pack_dir.join(idx_name)).expect("idx");
    assert_eq!(idx.find_offset(oid), Some(off));
}

pub fn batch_check_hex(repo: &Path, oids: &[ObjectId]) -> BatchCheckOutcome {
    let hex: Vec<String> = oids.iter().map(|o| o.to_hex().to_string()).collect();
    let refs: Vec<&str> = hex.iter().map(String::as_str).collect();
    git_cat_file_batch_check(repo, &refs)
}

pub fn odb_with_midx_config(objects: &Path, git_dir: &Path, enabled: bool) -> Odb {
    std::fs::write(
        git_dir.join("config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n\tmultiPackIndex = {}\n",
            if enabled { "true" } else { "false" }
        ),
    )
    .expect("write config");
    Odb::new(objects).with_config_git_dir(git_dir.to_path_buf())
}

pub fn read_chain_hashes(pack_dir: &Path) -> Option<Vec<String>> {
    let chain = pack_dir.join("multi-pack-index.d/multi-pack-index-chain");
    let text = std::fs::read_to_string(chain).ok()?;
    let hashes: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if hashes.is_empty() {
        None
    } else {
        Some(hashes)
    }
}

pub fn tip_midx_path(pack_dir: &Path) -> PathBuf {
    if let Ok(hashes) =
        std::fs::read_to_string(pack_dir.join("multi-pack-index.d/multi-pack-index-chain"))
    {
        let tip = hashes
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .last()
            .expect("non-empty chain");
        return pack_dir
            .join("multi-pack-index.d")
            .join(format!("multi-pack-index-{tip}.midx"));
    }
    pack_dir.join("multi-pack-index")
}

pub fn remove_loose_object(objects: &Path, oid: &ObjectId) {
    let hex = oid.to_hex();
    let loose = objects.join(&hex[..2]).join(&hex[2..]);
    let _ = std::fs::remove_file(loose);
}

const MIDX_SIG: u32 = 0x4d49_4458;

/// Locate a MIDX chunk by id in an on-disk image (`offset`, `length`).
pub fn find_midx_chunk(data: &[u8], id: u32) -> Option<(usize, usize)> {
    if data.len() < 12 {
        return None;
    }
    let sig = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    if sig != MIDX_SIG {
        return None;
    }
    let hdr_end = 12usize;
    let num_chunks = data[6] as usize;
    let toc_off = hdr_end;
    for i in 0..num_chunks {
        let entry = toc_off + i * 12;
        if entry + 12 > data.len() {
            return None;
        }
        let chunk_id = u32::from_be_bytes([
            data[entry],
            data[entry + 1],
            data[entry + 2],
            data[entry + 3],
        ]);
        let off = u64::from_be_bytes([
            data[entry + 4],
            data[entry + 5],
            data[entry + 6],
            data[entry + 7],
            data[entry + 8],
            data[entry + 9],
            data[entry + 10],
            data[entry + 11],
        ]) as usize;
        let next_entry = toc_off + (i + 1) * 12;
        if next_entry + 12 > data.len() {
            return None;
        }
        let next_off = u64::from_be_bytes([
            data[next_entry + 4],
            data[next_entry + 5],
            data[next_entry + 6],
            data[next_entry + 7],
            data[next_entry + 8],
            data[next_entry + 9],
            data[next_entry + 10],
            data[next_entry + 11],
        ]) as usize;
        if chunk_id == id {
            return Some((off, next_off.saturating_sub(off)));
        }
    }
    None
}

pub fn patch_midx_file(pack_dir: &Path, patch: impl FnOnce(&mut Vec<u8>)) {
    let path = tip_midx_path(pack_dir);
    let mut data = std::fs::read(&path).expect("read midx");
    patch(&mut data);
    grit_lib::midx::clear_pack_midx_state(pack_dir).expect("clear cache");
    std::fs::write(&path, &data).expect("write midx");
    clear_pack_cache();
}

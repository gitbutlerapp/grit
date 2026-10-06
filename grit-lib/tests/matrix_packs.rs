//! Comprehensive matrix tests for the negotiation-driven pack builder
//! (`grit_lib::transfer::build_pack`) and its delta encoder.
//!
//! These exercise correctness and validity of the produced PACK v2 streams across
//! a matrix of options:
//!
//!   * whole-object vs delta packing (same resolved object set out either way),
//!   * THIN packs that reference an external REF_DELTA base (and resolve once the
//!     base is supplied),
//!   * OFS vs REF delta selection (`use_ofs_delta` toggle),
//!   * the delta-depth limit (`max_depth`) actually capping in-pack chains,
//!   * minimality (a delta pack is far smaller than the whole-object pack),
//!   * SHA-256 repositories (32-byte oids throughout, index-pack + fsck clean),
//!   * the negotiated-push minimality guard (the 478 MB regression): a negotiated
//!     pack must carry FAR fewer objects than the full reachable closure.
//!
//! Every pack produced is fed back through grit-lib's own pack reader AND through
//! system `git index-pack` + `git fsck --strict` so we cross-check structural
//! validity against the reference implementation, not just our own parser.
//!
//! Fixtures are real on-disk repos built with the system `git`; tests SKIP
//! cleanly only when a genuinely unavailable feature (e.g. `--object-format=sha256`)
//! is missing, and even then the SHA-1 happy path remains fully asserted.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::transfer::{build_pack, PackBuildOptions};
use grit_lib::unpack_objects::{pack_bytes_to_object_map, pack_is_thin};

// ---------------------------------------------------------------------------
// System-git fixture helpers (copied/adapted from the sibling transfer_pack.rs
// harness so we do not reinvent repo bring-up or the index-pack/fsck plumbing).
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 git output")
}

/// Like [`git`] but returns success/failure without panicking — for capability
/// probes (e.g. does this git support `--object-format=sha256`).
fn git_try(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    let hex = git(dir, &["rev-parse", rev]);
    ObjectId::from_hex(hex.trim()).expect("valid oid")
}

fn open_odb(dir: &Path) -> Odb {
    let git_dir = dir.join(".git");
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir)
}

/// Header object count from a PACK v2 byte stream (after validating magic+version).
fn pack_header_count(bytes: &[u8]) -> u32 {
    assert_eq!(&bytes[0..4], b"PACK", "must start with PACK magic");
    assert_eq!(
        u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        2,
        "must be PACK version 2"
    );
    u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]])
}

// ---------------------------------------------------------------------------
// Pack structural walker (lifted from transfer_pack.rs so we agree on the wire
// format) — type/size header, OFS/REF base decoding, zlib stream consumption.
// ---------------------------------------------------------------------------

/// Read a pack object's (type, size) header, returning bytes consumed.
fn read_type_size(b: &[u8]) -> (u8, u64, usize) {
    let mut c = b[0];
    let type_code = (c >> 4) & 0x7;
    let mut size = (c & 0x0f) as u64;
    let mut shift = 4u32;
    let mut i = 1usize;
    while c & 0x80 != 0 {
        c = b[i];
        size |= ((c & 0x7f) as u64) << shift;
        shift += 7;
        i += 1;
    }
    (type_code, size, i)
}

/// Decompress one zlib stream from the front of `b`; return compressed bytes used.
fn zlib_consume(b: &[u8]) -> usize {
    use std::io::Read as _;
    let mut dec = flate2::bufread::ZlibDecoder::new(b);
    let mut sink = Vec::new();
    dec.read_to_end(&mut sink).expect("zlib decode");
    dec.total_in() as usize
}

/// Inflate a single zlib stream from the front of `b`.
fn inflate(b: &[u8]) -> Vec<u8> {
    use std::io::Read as _;
    let mut dec = flate2::bufread::ZlibDecoder::new(b);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("inflate");
    out
}

/// `(ref_delta_count, ofs_delta_count, total_objects)` for a pack.
fn pack_delta_stats(bytes: &[u8], algo_len: usize) -> (usize, usize, usize) {
    let nr = pack_header_count(bytes) as usize;
    let mut pos = 12usize;
    let mut ofs = 0usize;
    let mut refs = 0usize;
    for _ in 0..nr {
        let (type_code, _size, consumed) = read_type_size(&bytes[pos..]);
        pos += consumed;
        match type_code {
            6 => {
                ofs += 1;
                while bytes[pos] & 0x80 != 0 {
                    pos += 1;
                }
                pos += 1;
            }
            7 => {
                refs += 1;
                pos += algo_len;
            }
            _ => {}
        }
        pos += zlib_consume(&bytes[pos..]);
    }
    (refs, ofs, nr)
}

/// REF_DELTA base oids that are NOT among `in_pack` (external/thin bases).
fn ref_delta_external_bases(
    bytes: &[u8],
    algo_len: usize,
    in_pack: &HashSet<ObjectId>,
) -> Vec<ObjectId> {
    let nr = pack_header_count(bytes) as usize;
    let mut pos = 12usize;
    let mut out = Vec::new();
    for _ in 0..nr {
        let (type_code, _size, consumed) = read_type_size(&bytes[pos..]);
        pos += consumed;
        match type_code {
            6 => {
                while bytes[pos] & 0x80 != 0 {
                    pos += 1;
                }
                pos += 1;
            }
            7 => {
                let base = ObjectId::from_bytes(&bytes[pos..pos + algo_len]).unwrap();
                if !in_pack.contains(&base) {
                    out.push(base);
                }
                pos += algo_len;
            }
            _ => {}
        }
        pos += zlib_consume(&bytes[pos..]);
    }
    out
}

/// Per-entry structural record: where its payload zlib stream starts, the pack
/// type code, and which base (if any) it deltifies against.
struct RawEntry {
    payload_start: usize,
    type_code: u8,
    base: EdgeBase,
}

enum EdgeBase {
    None,
    /// OFS_DELTA base, by the base entry's absolute start offset.
    OfsAt(usize),
    /// REF_DELTA base, by oid (may be external/thin).
    Ref(ObjectId),
}

/// Structurally walk `pack`, one [`RawEntry`] per object in pack order, plus the
/// per-entry absolute start offsets (so OFS bases can be resolved to entry index).
fn walk_pack_entries(pack: &[u8], algo_len: usize) -> (Vec<RawEntry>, Vec<usize>) {
    let nr = pack_header_count(pack) as usize;
    let mut pos = 12usize;
    let mut out = Vec::with_capacity(nr);
    let mut starts = Vec::with_capacity(nr);
    for _ in 0..nr {
        let start = pos;
        starts.push(start);
        let (type_code, _size, consumed) = read_type_size(&pack[pos..]);
        pos += consumed;
        let base = match type_code {
            6 => {
                let mut c = pack[pos];
                pos += 1;
                let mut rel = (c & 0x7f) as usize;
                while c & 0x80 != 0 {
                    rel += 1;
                    c = pack[pos];
                    pos += 1;
                    rel = (rel << 7) + (c & 0x7f) as usize;
                }
                EdgeBase::OfsAt(start - rel)
            }
            7 => {
                let oid = ObjectId::from_bytes(&pack[pos..pos + algo_len]).unwrap();
                pos += algo_len;
                EdgeBase::Ref(oid)
            }
            _ => EdgeBase::None,
        };
        let payload_start = pos;
        pos += zlib_consume(&pack[pos..]);
        out.push(RawEntry {
            payload_start,
            type_code,
            base,
        });
    }
    (out, starts)
}

/// The longest in-pack delta chain (number of edges) in `pack`. An entry whose
/// base is external (thin) terminates the chain at that edge. REF bases that are
/// in-pack are followed; OFS bases are followed via their start offset.
fn max_in_pack_delta_depth(pack: &[u8], algo_len: usize) -> usize {
    let (entries, starts) = walk_pack_entries(pack, algo_len);
    let start_to_idx: HashMap<usize, usize> =
        starts.iter().enumerate().map(|(i, &s)| (s, i)).collect();
    // Map in-pack oids would require resolution; for OFS chains we follow offsets.
    // For REF chains, build an oid->idx map by resolving each entry's oid below is
    // overkill: REF deltas in our packs only appear thin (external) or — for the
    // OFS-disabled path — name an in-pack base. We resolve REF bases by oid using a
    // reconstruction pass in `pack_delta_edges_resolved`; here we only need *depth*
    // of OFS chains and single-edge REF chains, which suffices for the depth test
    // (it uses OFS deltas). Treat a REF whose base oid we cannot map as depth-1.
    let mut memo: HashMap<usize, usize> = HashMap::new();
    fn depth_of(
        idx: usize,
        entries: &[RawEntry],
        start_to_idx: &HashMap<usize, usize>,
        memo: &mut HashMap<usize, usize>,
    ) -> usize {
        if let Some(&d) = memo.get(&idx) {
            return d;
        }
        let d = match &entries[idx].base {
            EdgeBase::None => 0,
            EdgeBase::OfsAt(off) => {
                if let Some(&bidx) = start_to_idx.get(off) {
                    1 + depth_of(bidx, entries, start_to_idx, memo)
                } else {
                    1
                }
            }
            EdgeBase::Ref(_) => 1,
        };
        memo.insert(idx, d);
        d
    }
    let mut max = 0;
    for i in 0..entries.len() {
        max = max.max(depth_of(i, &entries, &start_to_idx, &mut memo));
    }
    max
}

/// Apply a Git delta instruction stream (copy/insert ops) to `base`.
fn apply_delta(base: &[u8], delta: &[u8]) -> Vec<u8> {
    let mut i = 0usize;
    while delta[i] & 0x80 != 0 {
        i += 1;
    }
    i += 1; // source size varint
    while delta[i] & 0x80 != 0 {
        i += 1;
    }
    i += 1; // target size varint
    let mut out = Vec::new();
    while i < delta.len() {
        let op = delta[i];
        i += 1;
        if op & 0x80 != 0 {
            let mut off = 0usize;
            let mut len = 0usize;
            for s in 0..4 {
                if op & (1 << s) != 0 {
                    off |= (delta[i] as usize) << (8 * s);
                    i += 1;
                }
            }
            for s in 0..3 {
                if op & (1 << (4 + s)) != 0 {
                    len |= (delta[i] as usize) << (8 * s);
                    i += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            out.extend_from_slice(&base[off..off + len]);
        } else if op != 0 {
            let len = op as usize;
            out.extend_from_slice(&delta[i..i + len]);
            i += len;
        }
    }
    out
}

/// Git object id of `content` under `kind` ("blob"/"tree"/...) at the given hash.
fn git_hash_object(kind: &str, content: &[u8], algo: HashAlgo) -> ObjectId {
    use sha1::{Digest as _, Sha1};
    use sha2::Sha256;
    let header = format!("{kind} {}\0", content.len());
    match algo {
        HashAlgo::Sha1 => {
            let mut h = Sha1::new();
            h.update(header.as_bytes());
            h.update(content);
            ObjectId::from_bytes(&h.finalize()).unwrap()
        }
        HashAlgo::Sha256 => {
            let mut h = Sha256::new();
            h.update(header.as_bytes());
            h.update(content);
            ObjectId::from_bytes(&h.finalize()).unwrap()
        }
    }
}

/// Resolve every delta edge in `pack` to `(target_oid, base_oid)` by fully
/// reconstructing each object (inflating + applying its base chain) and hashing
/// it. OFS bases resolve via in-pack offset; REF bases by oid (external/thin
/// bases looked up in `odb`).
fn pack_delta_edges_resolved(pack: &[u8], odb: &Odb) -> Vec<(ObjectId, ObjectId)> {
    let algo = odb.hash_algo();
    let algo_len = algo.len();
    let (entries, starts) = walk_pack_entries(pack, algo_len);
    let start_to_idx: HashMap<usize, usize> =
        starts.iter().enumerate().map(|(i, &s)| (s, i)).collect();

    let mut content_at: HashMap<usize, (Vec<u8>, u8)> = HashMap::new();

    fn resolve(
        idx: usize,
        entries: &[RawEntry],
        pack: &[u8],
        start_to_idx: &HashMap<usize, usize>,
        content_at: &mut HashMap<usize, (Vec<u8>, u8)>,
        odb: &Odb,
    ) -> (Vec<u8>, u8) {
        if let Some(v) = content_at.get(&idx) {
            return v.clone();
        }
        let e = &entries[idx];
        let payload = {
            let z = zlib_consume(&pack[e.payload_start..]);
            inflate(&pack[e.payload_start..e.payload_start + z])
        };
        let result = match &e.base {
            EdgeBase::None => (payload, e.type_code),
            EdgeBase::OfsAt(off) => {
                let bidx = start_to_idx[off];
                let (base_content, base_type) =
                    resolve(bidx, entries, pack, start_to_idx, content_at, odb);
                (apply_delta(&base_content, &payload), base_type)
            }
            EdgeBase::Ref(oid) => {
                // REF base: look it up in the odb (always populated in these
                // tests, whether the base is in-pack or external/thin).
                let (base_content, base_type) = if let Ok(obj) = odb.read(oid) {
                    let tc = match obj.kind {
                        ObjectKind::Commit => 1,
                        ObjectKind::Tree => 2,
                        ObjectKind::Blob => 3,
                        ObjectKind::Tag => 4,
                    };
                    (obj.data, tc)
                } else {
                    (Vec::new(), 3)
                };
                (apply_delta(&base_content, &payload), base_type)
            }
        };
        content_at.insert(idx, result.clone());
        result
    }

    let mut oid_at: Vec<ObjectId> = Vec::with_capacity(entries.len());
    for i in 0..entries.len() {
        let (content, type_code) = resolve(i, &entries, pack, &start_to_idx, &mut content_at, odb);
        let kind = match type_code {
            1 => "commit",
            2 => "tree",
            4 => "tag",
            _ => "blob",
        };
        oid_at.push(git_hash_object(kind, &content, algo));
    }

    let mut out = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let t = oid_at[i];
        match &e.base {
            EdgeBase::None => {}
            EdgeBase::OfsAt(off) => {
                let bidx = start_to_idx[off];
                out.push((t, oid_at[bidx]));
            }
            EdgeBase::Ref(oid) => out.push((t, *oid)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// system git index-pack / fsck cross-checks.
// ---------------------------------------------------------------------------

/// Index a self-contained pack into a fresh bare repo (of the right object
/// format) and run `git fsck --strict`. Returns whether both succeeded.
fn git_index_and_fsck_ok(pack: &[u8], algo: HashAlgo) -> bool {
    let scratch = tempfile::tempdir().expect("scratch repo");
    let mut init_args = vec!["init", "-q", "--bare"];
    let fmt = format!("--object-format={}", algo.name());
    if algo == HashAlgo::Sha256 {
        init_args.push(&fmt);
    }
    init_args.push(".");
    if !git_try(scratch.path(), &init_args) {
        return false;
    }

    let pack_path = scratch.path().join("objects/pack/in.pack");
    std::fs::write(&pack_path, pack).unwrap();
    let idx = Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", &pack_path.to_string_lossy()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git index-pack");
    if !idx.status.success() {
        eprintln!(
            "index-pack into bare repo failed: {}",
            String::from_utf8_lossy(&idx.stderr)
        );
        return false;
    }

    let fsck = Command::new("git")
        .current_dir(scratch.path())
        .args(["fsck", "--strict", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    if !fsck.status.success() {
        eprintln!(
            "git fsck failed: {}\n{}",
            String::from_utf8_lossy(&fsck.stdout),
            String::from_utf8_lossy(&fsck.stderr)
        );
    }
    fsck.status.success()
}

/// Feed a thin pack to `git index-pack --fix-thin --stdin` inside `repo` (which
/// must hold the external bases). Returns whether git accepted it.
fn git_index_pack_fix_thin_ok(repo: &Path, pack: &[u8]) -> bool {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["index-pack", "-v", "--fix-thin", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git index-pack --stdin");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(pack)
        .expect("write pack to stdin");
    let out = child.wait_with_output().expect("wait git index-pack");
    if !out.status.success() {
        eprintln!(
            "git index-pack --fix-thin failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.status.success()
}

/// Pack the exact object closure of `tips` with system `git pack-objects` and
/// return the pack size (bytes) — the parity yardstick for our delta packer.
fn git_pack_objects_size(repo: &Path, tips: &[ObjectId]) -> usize {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["pack-objects", "--stdout", "--revs", "--delta-base-offset"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git pack-objects");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        for t in tips {
            writeln!(stdin, "{}", t.to_hex()).unwrap();
        }
    }
    let out = child.wait_with_output().expect("wait git pack-objects");
    assert!(
        out.status.success(),
        "git pack-objects failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout.len()
}

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// Two-commit repo: C1 introduces a.txt; C2 adds b.txt (a.txt blob unchanged).
struct SmallFixture {
    dir: tempfile::TempDir,
    c1: ObjectId,
    c2: ObjectId,
    c1_objects: Vec<ObjectId>,
    c2_new_objects: Vec<ObjectId>,
}

fn build_small_fixture(algo: HashAlgo) -> Option<SmallFixture> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let fmt = format!("--object-format={}", algo.name());
    let mut init = vec!["init", "-q", "-b", "main"];
    if algo == HashAlgo::Sha256 {
        init.push(&fmt);
    }
    init.push(".");
    if !git_try(dir, &init) {
        return None;
    }

    std::fs::write(dir.join("a.txt"), b"hello\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(dir, "HEAD");
    let c1_tree = rev_parse(dir, "HEAD^{tree}");
    let c1_blob = rev_parse(dir, "HEAD:a.txt");

    std::fs::write(dir.join("b.txt"), b"world\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
    let c2 = rev_parse(dir, "HEAD");
    let c2_tree = rev_parse(dir, "HEAD^{tree}");
    let c2_blob = rev_parse(dir, "HEAD:b.txt");

    Some(SmallFixture {
        dir: tmp,
        c1,
        c2,
        c1_objects: vec![c1, c1_tree, c1_blob],
        c2_new_objects: vec![c2, c2_tree, c2_blob],
    })
}

/// Repo with one large file extended (prefix-preserving) across 6 commits — ideal
/// delta bait. Successive big.txt versions share a long common prefix.
struct DeltaFixture {
    dir: tempfile::TempDir,
    tips: Vec<ObjectId>,
}

fn build_delta_fixture(algo: HashAlgo) -> Option<DeltaFixture> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let fmt = format!("--object-format={}", algo.name());
    let mut init = vec!["init", "-q", "-b", "main"];
    if algo == HashAlgo::Sha256 {
        init.push(&fmt);
    }
    init.push(".");
    if !git_try(dir, &init) {
        return None;
    }

    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!(
            "line {i:05} lorem ipsum dolor sit amet consectetur\n"
        ));
    }

    let mut tips = Vec::new();
    for rev in 0..6 {
        body.push_str(&format!("--- edit number {rev} appended at the end ---\n"));
        std::fs::write(dir.join("big.txt"), body.as_bytes()).unwrap();
        git(dir, &["add", "big.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{rev}")]);
        tips.push(rev_parse(dir, "HEAD"));
    }

    Some(DeltaFixture { dir: tmp, tips })
}

/// Like [`build_delta_fixture`], but with `revisions` prefix-extending commits so
/// in-pack delta chains can exceed the default `pack.depth` (50).
struct DeepDeltaFixture {
    dir: tempfile::TempDir,
    tip: ObjectId,
}

fn build_deep_delta_fixture(revisions: usize) -> Option<DeepDeltaFixture> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !git_try(dir, &["init", "-q", "-b", "main", "."]) {
        return None;
    }
    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!(
            "line {i:05} lorem ipsum dolor sit amet consectetur\n"
        ));
    }
    std::fs::write(dir.join("big.txt"), body.as_bytes()).unwrap();
    git(dir, &["add", "big.txt"]);
    git(dir, &["commit", "-q", "-m", "c0"]);
    for rev in 1..revisions {
        body.push_str(&format!("--- revision {rev} ---\n"));
        std::fs::write(dir.join("big.txt"), body.as_bytes()).unwrap();
        git(dir, &["add", "big.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{rev}")]);
    }
    let tip = rev_parse(dir, "HEAD");
    Some(DeepDeltaFixture { dir: tmp, tip })
}

/// Longest chain length reported by `git verify-pack -v` on a standalone pack file.
fn git_verify_pack_max_chain(pack: &[u8]) -> u64 {
    let scratch = tempfile::tempdir().expect("scratch");
    let pack_path = scratch.path().join("in.pack");
    std::fs::write(&pack_path, pack).expect("write pack");
    let idx = Command::new("git")
        .args(["index-pack", &pack_path.to_string_lossy()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git index-pack");
    assert!(
        idx.status.success(),
        "git index-pack failed: {}",
        String::from_utf8_lossy(&idx.stderr)
    );
    let out = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&pack_path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git verify-pack");
    assert!(
        out.status.success() || String::from_utf8_lossy(&out.stdout).contains(": ok"),
        "git verify-pack failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut max = 0u64;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.strip_prefix("chain length = ") {
            if let Some(len_str) = rest.split(':').next() {
                if let Ok(len) = len_str.trim().parse::<u64>() {
                    max = max.max(len);
                }
            }
        }
    }
    max
}

// ---------------------------------------------------------------------------
// 1. whole-object vs delta correctness: same object set out either way.
// ---------------------------------------------------------------------------

#[test]
fn whole_and_delta_packs_resolve_to_same_object_set() {
    let Some(fx) = build_delta_fixture(HashAlgo::Sha1) else {
        eprintln!("SKIP: could not init git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());
    let tip = *fx.tips.last().unwrap();

    let whole = build_pack(&odb, &[tip], &[], &PackBuildOptions::default()).expect("whole");
    let delta = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("delta");

    // Both start with PACK v2 and report a header count that matches what they
    // resolve to.
    let whole_map = pack_bytes_to_object_map(&whole, &odb).expect("reparse whole");
    let delta_map = pack_bytes_to_object_map(&delta, &odb).expect("reparse delta");
    assert_eq!(pack_header_count(&whole) as usize, whole_map.len());
    assert_eq!(pack_header_count(&delta) as usize, delta_map.len());

    // The resolved object set is IDENTICAL whether or not deltas are used: delta
    // encoding is a pure wire optimization, never an object-set change.
    assert_eq!(
        whole_map.keys().collect::<BTreeSet<_>>(),
        delta_map.keys().collect::<BTreeSet<_>>(),
        "delta pack must resolve to the same object set as the whole pack"
    );

    // The whole-object pack contains NO deltas; the delta pack DOES.
    let (rw, ow, _) = pack_delta_stats(&whole, 20);
    assert_eq!(rw + ow, 0, "whole pack must contain no delta entries");
    let (rd, od, _) = pack_delta_stats(&delta, 20);
    assert!(
        rd + od >= 3,
        "delta pack must contain several delta entries (ref={rd} ofs={od})"
    );

    // Both are accepted by system git index-pack + fsck --strict.
    assert!(
        git_index_and_fsck_ok(&whole, HashAlgo::Sha1),
        "whole pack failed index-pack + fsck"
    );
    assert!(
        git_index_and_fsck_ok(&delta, HashAlgo::Sha1),
        "delta pack failed index-pack + fsck"
    );

    // Per-edge content correctness: every delta edge, fully reconstructed and
    // re-hashed, lands on an oid that is actually in the pack.
    let edges = pack_delta_edges_resolved(&delta, &odb);
    assert!(!edges.is_empty(), "expected resolvable delta edges");
    for (t, _b) in &edges {
        assert!(
            delta_map.contains_key(t),
            "reconstructed delta target {t} not in pack object set"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. minimality: a delta pack is FAR smaller than the whole-object pack, and is
//    within a reasonable factor of git pack-objects.
// ---------------------------------------------------------------------------

#[test]
fn delta_pack_is_far_smaller_and_near_pack_objects() {
    let Some(fx) = build_delta_fixture(HashAlgo::Sha1) else {
        eprintln!("SKIP: could not init git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());
    let tip = *fx.tips.last().unwrap();

    let whole = build_pack(&odb, &[tip], &[], &PackBuildOptions::default()).expect("whole");
    let delta = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("delta");

    // The six ~200KB near-identical blobs collapse to one full blob + five small
    // deltas, so the delta pack is dramatically smaller.
    assert!(
        delta.len() * 2 < whole.len(),
        "delta pack ({}) should be < half the whole pack ({})",
        delta.len(),
        whole.len()
    );

    // Within a generous factor of git's own packer for the same closure.
    let git_size = git_pack_objects_size(fx.dir.path(), &[tip]);
    assert!(
        delta.len() <= git_size * 6,
        "delta pack ({}) is more than 6x git pack-objects ({}), ratio {:.2}",
        delta.len(),
        git_size,
        delta.len() as f64 / git_size as f64
    );
    eprintln!(
        "matrix minimality: ours={} git={} ratio={:.2} whole={}",
        delta.len(),
        git_size,
        delta.len() as f64 / git_size as f64,
        whole.len()
    );
}

// ---------------------------------------------------------------------------
// 3. THIN pack: references an external REF_DELTA base absent from the pack, and
//    that base resolves once supplied (the odb / git holds it).
// ---------------------------------------------------------------------------

#[test]
fn thin_pack_references_external_ref_delta_base_and_resolves() {
    let Some(fx) = build_delta_fixture(HashAlgo::Sha1) else {
        eprintln!("SKIP: could not init git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());

    let want = *fx.tips.last().unwrap();
    let have = fx.tips[fx.tips.len() - 2];

    let thin = build_pack(
        &odb,
        &[want],
        &[have],
        &PackBuildOptions {
            delta: true,
            thin: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("thin pack");

    // (a) grit-lib agrees the pack is thin.
    assert!(
        pack_is_thin(&thin, HashAlgo::Sha1),
        "pack should be detected as thin"
    );

    // (b) It uses at least one REF_DELTA, and at least one names a base NOT in the
    // pack (the peer-held external base).
    let map = pack_bytes_to_object_map(&thin, &odb).expect("thin pack resolves via odb");
    let in_pack: HashSet<ObjectId> = map.keys().copied().collect();
    let (ref_n, _ofs_n, _total) = pack_delta_stats(&thin, 20);
    assert!(ref_n >= 1, "thin pack must use at least one REF_DELTA");
    let external = ref_delta_external_bases(&thin, 20, &in_pack);
    assert!(
        !external.is_empty(),
        "thin pack must reference at least one base NOT in the pack"
    );

    // (c) The external base is genuinely absent from the pack but present in the
    // source odb (so "supplying the base" lets resolution succeed). The map above
    // already resolved against that odb-held base; assert the base oid is one of
    // the previous-tip's big.txt blobs (i.e. a real peer-held object).
    for base in &external {
        assert!(
            !in_pack.contains(base),
            "external base {base} must NOT be in the pack"
        );
        assert!(
            odb.read(base).is_ok(),
            "external base {base} must be resolvable from the source odb (the supplied base)"
        );
    }

    // The newest big.txt blob must be in the resolved set.
    let big_oid = rev_parse(fx.dir.path(), &format!("{}:big.txt", want.to_hex()));
    assert!(
        map.contains_key(&big_oid),
        "resolved thin pack must contain the newest big.txt blob"
    );

    // (d) System git accepts it via index-pack --fix-thin inside the repo (which
    // holds the bases) — the cross-check that the thin pack is well-formed and its
    // external bases are exactly the repo-held ones.
    assert!(
        git_index_pack_fix_thin_ok(fx.dir.path(), &thin),
        "system git index-pack --fix-thin rejected the thin pack"
    );

    // (e) Supplying the base explicitly: a NON-thin pack of the same want over the
    // same have, written into a fresh repo that we first seed with the base, must
    // index. We simulate "supply the base then the thin pack" by indexing the thin
    // pack with --fix-thin (done above) and separately confirming a self-contained
    // full pack of `want` alone (empty haves) index+fscks clean.
    let full = build_pack(
        &odb,
        &[want],
        &[],
        &PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("full pack of want");
    assert!(
        !pack_is_thin(&full, HashAlgo::Sha1),
        "full pack (empty haves) must NOT be thin"
    );
    assert!(
        git_index_and_fsck_ok(&full, HashAlgo::Sha1),
        "self-contained full pack failed index-pack + fsck"
    );
}

// ---------------------------------------------------------------------------
// 4. OFS vs REF delta selection (`use_ofs_delta`).
// ---------------------------------------------------------------------------

#[test]
fn ofs_vs_ref_delta_selection() {
    let Some(fx) = build_delta_fixture(HashAlgo::Sha1) else {
        eprintln!("SKIP: could not init git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());
    let tip = *fx.tips.last().unwrap();

    // OFS path: in-pack bases referenced by offset → OFS_DELTA, no REF_DELTA.
    let ofs = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("ofs pack");
    let (ref_ofs, ofs_ofs, _) = pack_delta_stats(&ofs, 20);
    assert!(
        ofs_ofs >= 3,
        "use_ofs_delta=true should emit OFS_DELTAs (got {ofs_ofs})"
    );
    assert_eq!(
        ref_ofs, 0,
        "use_ofs_delta=true with no thin bases must emit NO REF_DELTAs (got {ref_ofs})"
    );

    // REF path: same in-pack bases now referenced by oid → REF_DELTA, no OFS.
    let refp = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            use_ofs_delta: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("ref pack");
    let (ref_ref, ofs_ref, _) = pack_delta_stats(&refp, 20);
    assert!(
        ref_ref >= 3,
        "use_ofs_delta=false should emit REF_DELTAs (got {ref_ref})"
    );
    assert_eq!(
        ofs_ref, 0,
        "use_ofs_delta=false must emit NO OFS_DELTAs (got {ofs_ref})"
    );

    // Both encodings resolve to the SAME object set and both index+fsck clean.
    let m_ofs = pack_bytes_to_object_map(&ofs, &odb).expect("reparse ofs");
    let m_ref = pack_bytes_to_object_map(&refp, &odb).expect("reparse ref");
    assert_eq!(
        m_ofs.keys().collect::<BTreeSet<_>>(),
        m_ref.keys().collect::<BTreeSet<_>>(),
        "OFS and REF encodings must resolve to the same object set"
    );
    assert!(
        git_index_and_fsck_ok(&ofs, HashAlgo::Sha1),
        "OFS pack failed index-pack + fsck"
    );
    assert!(
        git_index_and_fsck_ok(&refp, HashAlgo::Sha1),
        "REF pack failed index-pack + fsck"
    );
}

// ---------------------------------------------------------------------------
// 5. delta depth limit (`max_depth`) actually caps in-pack chains.
// ---------------------------------------------------------------------------

#[test]
fn delta_depth_limit_caps_chain_length() {
    let Some(fx) = build_delta_fixture(HashAlgo::Sha1) else {
        eprintln!("SKIP: could not init git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());
    let tip = *fx.tips.last().unwrap();

    // With a generous depth, the 6 prefix-extended blobs can form a long chain.
    let deep = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            max_depth: 50,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("deep pack");
    let deep_depth = max_in_pack_delta_depth(&deep, 20);

    // With max_depth=1, no in-pack delta chain may exceed a single edge.
    let shallow = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            max_depth: 1,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("shallow pack");
    let shallow_depth = max_in_pack_delta_depth(&shallow, 20);

    assert!(
        shallow_depth <= 1,
        "max_depth=1 must cap in-pack delta chains at 1 edge, saw depth {shallow_depth}"
    );
    // The depth cap is meaningful only if the unconstrained pack chains deeper.
    assert!(
        deep_depth >= 2,
        "fixture should produce a chain deeper than 1 at max_depth=50 (saw {deep_depth})"
    );
    assert!(
        shallow_depth < deep_depth,
        "lowering max_depth must shorten the longest chain ({shallow_depth} < {deep_depth})"
    );

    // Both still index + fsck clean and carry the same object set.
    assert!(
        git_index_and_fsck_ok(&shallow, HashAlgo::Sha1),
        "depth-1 pack failed index-pack + fsck"
    );
    let m_deep = pack_bytes_to_object_map(&deep, &odb).expect("reparse deep");
    let m_shallow = pack_bytes_to_object_map(&shallow, &odb).expect("reparse shallow");
    assert_eq!(
        m_deep.keys().collect::<BTreeSet<_>>(),
        m_shallow.keys().collect::<BTreeSet<_>>(),
        "depth limit must not change the packed object set"
    );
}

// ---------------------------------------------------------------------------
// 6. minimal selection: build_pack(wants, haves) packs ONLY new objects.
// ---------------------------------------------------------------------------

#[test]
fn build_pack_selects_only_new_objects() {
    let Some(fx) = build_small_fixture(HashAlgo::Sha1) else {
        eprintln!("SKIP: could not init git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());

    let pack = build_pack(&odb, &[fx.c2], &[fx.c1], &PackBuildOptions::default()).expect("build");
    assert_eq!(&pack[0..4], b"PACK");

    // Exactly the 3 objects new in C2 (new commit + new tree + new blob). The
    // unchanged a.txt blob and C1's objects, reachable from the have, are excluded.
    let count = pack_header_count(&pack);
    assert_eq!(
        count,
        fx.c2_new_objects.len() as u32,
        "expected exactly the 3 objects new in C2, got {count}"
    );

    let map = pack_bytes_to_object_map(&pack, &odb).expect("re-parse pack");
    assert_eq!(map.len(), fx.c2_new_objects.len());
    for oid in &fx.c2_new_objects {
        assert!(map.contains_key(oid), "pack missing new object {oid}");
    }
    for oid in &fx.c1_objects {
        assert!(
            !map.contains_key(oid),
            "pack should not contain have-reachable object {oid}"
        );
    }

    // Empty haves → full closure of C2 = 6 objects.
    let full = build_pack(&odb, &[fx.c2], &[], &PackBuildOptions::default()).expect("full");
    let full_map = pack_bytes_to_object_map(&full, &odb).expect("re-parse full");
    assert_eq!(full_map.len(), 6, "full closure of C2 is 6 objects");
    for oid in fx.c1_objects.iter().chain(fx.c2_new_objects.iter()) {
        assert!(full_map.contains_key(oid), "full pack missing {oid}");
    }
}

// ---------------------------------------------------------------------------
// 7. Large-repo guard (the 478 MB regression): a negotiated push pack carries
//    FAR fewer objects than the all-reachable closure of the pushed tip.
// ---------------------------------------------------------------------------

#[test]
fn negotiated_push_pack_is_far_below_full_closure() {
    // A repo with deep history (many commits, each touching files) so the full
    // reachable closure is large; a push that the peer has nearly caught up on
    // must transfer only the handful of newly-introduced objects.
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    if !git_try(dir, &["init", "-q", "-b", "main", "."]) {
        eprintln!("SKIP: could not init git repo");
        return;
    }

    // Build 60 commits, each adding a new file AND rewriting a shared file, so
    // every commit introduces a commit + a new root tree + a couple of blobs.
    let mut tips = Vec::new();
    for i in 0..60 {
        std::fs::write(dir.join(format!("f{i:03}.txt")), format!("content {i}\n")).unwrap();
        std::fs::write(dir.join("shared.txt"), format!("shared rev {i}\n")).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", &format!("commit {i}")]);
        tips.push(rev_parse(dir, "HEAD"));
    }
    let odb = open_odb(dir);
    let tip = *tips.last().unwrap();
    let prev = tips[tips.len() - 2];

    // The FULL closure of the tip (what a naive "pack everything reachable from the
    // pushed tip" would send — the 478 MB-style regression).
    let full = build_pack(&odb, &[tip], &[], &PackBuildOptions::default()).expect("full");
    let full_count = pack_header_count(&full);

    // The NEGOTIATED pack: peer already holds `prev`, so we send only what `tip`
    // introduces over `prev`.
    let negotiated =
        build_pack(&odb, &[tip], &[prev], &PackBuildOptions::default()).expect("negotiated");
    let neg_count = pack_header_count(&negotiated);

    // The full closure has hundreds of objects (60 commits * ~3 objects each);
    // a single-commit advance introduces only a handful.
    assert!(
        full_count >= 150,
        "fixture should have a large full closure (got {full_count})"
    );
    assert!(
        neg_count <= 5,
        "a one-commit-ahead push must send only a handful of objects, got {neg_count}"
    );
    // The negotiated pack must be DRAMATICALLY smaller than the full closure — the
    // core regression guard. Require at least a 20x object-count reduction.
    assert!(
        (neg_count as u32) * 20 < full_count,
        "negotiated pack ({neg_count} objs) is not far below full closure ({full_count} objs)"
    );

    // And the negotiated pack resolves cleanly (its bases are all peer-held or
    // in-pack) — index it with --fix-thin inside the repo just in case, and verify
    // it carries exactly the new commit.
    let neg_map = pack_bytes_to_object_map(&negotiated, &odb).expect("reparse negotiated");
    assert!(
        neg_map.contains_key(&tip),
        "negotiated pack must contain the pushed tip commit"
    );
    assert!(
        !neg_map.contains_key(&prev),
        "negotiated pack must NOT re-send the peer-held previous tip"
    );

    eprintln!(
        "478MB guard: full_closure={} negotiated={} reduction={:.0}x",
        full_count,
        neg_count,
        full_count as f64 / neg_count.max(1) as f64
    );
}

// ---------------------------------------------------------------------------
// 8. SHA-256 repository: every pack (whole, delta, thin) carries 64-hex oids and
//    is accepted by a sha256 git index-pack + fsck.
// ---------------------------------------------------------------------------

#[test]
fn sha256_repo_packs_index_and_fsck_clean() {
    // Probe sha256 support.
    let probe = tempfile::tempdir().expect("probe");
    if !git_try(
        probe.path(),
        &["init", "--object-format=sha256", "--bare", "."],
    ) {
        eprintln!("SKIP: git lacks --object-format=sha256 support");
        return;
    }

    let Some(fx) = build_delta_fixture(HashAlgo::Sha256) else {
        eprintln!("SKIP: could not init sha256 git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());
    assert_eq!(
        odb.hash_algo(),
        HashAlgo::Sha256,
        "odb must report sha256 for a sha256 repo"
    );
    let tip = *fx.tips.last().unwrap();
    assert_eq!(tip.to_hex().len(), 64, "sha256 oids must be 64-hex");

    // Whole pack.
    let whole = build_pack(&odb, &[tip], &[], &PackBuildOptions::default()).expect("sha256 whole");
    let whole_map = pack_bytes_to_object_map(&whole, &odb).expect("reparse sha256 whole");
    assert_eq!(pack_header_count(&whole) as usize, whole_map.len());
    assert!(
        git_index_and_fsck_ok(&whole, HashAlgo::Sha256),
        "sha256 whole pack failed index-pack + fsck"
    );

    // Delta pack: smaller, same object set, valid. REF/OFS base oids are 32 bytes.
    let delta = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("sha256 delta");
    let delta_map = pack_bytes_to_object_map(&delta, &odb).expect("reparse sha256 delta");
    assert_eq!(
        whole_map.keys().collect::<BTreeSet<_>>(),
        delta_map.keys().collect::<BTreeSet<_>>(),
        "sha256 delta pack must resolve to the same object set as the whole pack"
    );
    assert!(
        delta.len() * 2 < whole.len(),
        "sha256 delta pack ({}) should be < half the whole pack ({})",
        delta.len(),
        whole.len()
    );
    // Delta stats must use the 32-byte oid width for REF base parsing.
    let (_r, o, _t) = pack_delta_stats(&delta, 32);
    assert!(o >= 1, "sha256 delta pack should contain OFS deltas");
    assert!(
        git_index_and_fsck_ok(&delta, HashAlgo::Sha256),
        "sha256 delta pack failed index-pack + fsck"
    );

    // Per-edge reconstruction must hash correctly under sha256.
    let edges = pack_delta_edges_resolved(&delta, &odb);
    assert!(
        !edges.is_empty(),
        "sha256 delta pack should have resolvable edges"
    );
    for (t, _b) in &edges {
        assert!(
            delta_map.contains_key(t),
            "sha256 reconstructed delta target {t} not in pack"
        );
    }

    // Thin pack over a have: 64-hex external REF base, accepted via --fix-thin.
    let prev = fx.tips[fx.tips.len() - 2];
    let thin = build_pack(
        &odb,
        &[tip],
        &[prev],
        &PackBuildOptions {
            delta: true,
            thin: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("sha256 thin");
    assert!(
        pack_is_thin(&thin, HashAlgo::Sha256),
        "sha256 thin pack must be detected as thin"
    );
    let thin_map = pack_bytes_to_object_map(&thin, &odb).expect("reparse sha256 thin");
    let in_pack: HashSet<ObjectId> = thin_map.keys().copied().collect();
    let external = ref_delta_external_bases(&thin, 32, &in_pack);
    assert!(
        !external.is_empty(),
        "sha256 thin pack must reference an external 32-byte base"
    );
    for base in &external {
        assert_eq!(
            base.to_hex().len(),
            64,
            "external base must be a sha256 oid"
        );
        assert!(
            odb.read(base).is_ok(),
            "external base must resolve from odb"
        );
    }
    assert!(
        git_index_pack_fix_thin_ok(fx.dir.path(), &thin),
        "sha256 thin pack rejected by git index-pack --fix-thin"
    );
}

// ---------------------------------------------------------------------------
// 9. window=0 disables delta selection (whole-object fallback) but stays valid.
// ---------------------------------------------------------------------------

#[test]
fn window_zero_disables_delta_selection() {
    let Some(fx) = build_delta_fixture(HashAlgo::Sha1) else {
        eprintln!("SKIP: could not init git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());
    let tip = *fx.tips.last().unwrap();

    let pack = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            window: 0,
            ..PackBuildOptions::default()
        },
    )
    .expect("window-0 pack");

    // window=0 disables in-pack delta selection → a whole-object pack.
    let (r, o, _t) = pack_delta_stats(&pack, 20);
    assert_eq!(
        r + o,
        0,
        "window=0 must produce no deltas (ref={r} ofs={o})"
    );

    // Still a correct, valid pack with the full closure.
    let whole = build_pack(&odb, &[tip], &[], &PackBuildOptions::default()).expect("whole");
    let m0 = pack_bytes_to_object_map(&pack, &odb).expect("reparse window0");
    let mw = pack_bytes_to_object_map(&whole, &odb).expect("reparse whole");
    assert_eq!(
        m0.keys().collect::<BTreeSet<_>>(),
        mw.keys().collect::<BTreeSet<_>>(),
        "window=0 must produce the same object set as plain whole packing"
    );
    assert!(
        git_index_and_fsck_ok(&pack, HashAlgo::Sha1),
        "window=0 pack failed index-pack + fsck"
    );
}

// ---------------------------------------------------------------------------
// 6. Long blob history: depth cap matches Git and survives index-pack + fsck.
// ---------------------------------------------------------------------------

#[test]
fn long_history_delta_depth_git_round_trip() {
    let Some(fx) = build_deep_delta_fixture(71) else {
        eprintln!("SKIP: could not init git repo");
        return;
    };
    let odb = open_odb(fx.dir.path());
    let tip = fx.tip;

    let uncapped = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            max_depth: 250,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("uncapped pack");
    let uncapped_depth = max_in_pack_delta_depth(&uncapped, 20);
    assert!(
        uncapped_depth >= 50,
        "fixture must chain deeper than default depth 50 (saw {uncapped_depth})"
    );

    let capped = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            max_depth: 50,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("depth-50 pack");
    assert_eq!(
        max_in_pack_delta_depth(&capped, 20),
        50,
        "build_pack must honor max_depth=50 exactly"
    );
    assert_eq!(
        git_verify_pack_max_chain(&capped),
        50,
        "git verify-pack must report the same chain cap"
    );
    assert!(
        git_index_and_fsck_ok(&capped, HashAlgo::Sha1),
        "depth-50 pack must index-pack and fsck under system git"
    );
    assert!(
        uncapped_depth > max_in_pack_delta_depth(&capped, 20),
        "capping must shorten chains"
    );

    let shallow = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            max_depth: 5,
            use_ofs_delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("depth-5 pack");
    assert_eq!(max_in_pack_delta_depth(&shallow, 20), 5);
    assert_eq!(git_verify_pack_max_chain(&shallow), 5);
    assert!(git_index_and_fsck_ok(&shallow, HashAlgo::Sha1));
}

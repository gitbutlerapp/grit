//! Integration tests for the negotiation-driven pack builder
//! (`grit_lib::transfer::build_pack`).
//!
//! These build a tiny repo with the system `git`, then assert that
//! `build_pack(wants, haves)` selects *only* the objects newly introduced by the
//! wanted tip (the minimal-selection regression guard for the 478 MB finding),
//! and that the produced bytes are a structurally valid PACK v2 stream.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::transfer::{build_pack, PackBuildOptions};
use grit_lib::unpack_objects::pack_bytes_to_object_map;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
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

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    let hex = git(dir, &["rev-parse", rev]);
    ObjectId::from_hex(hex.trim()).expect("valid oid")
}

/// Header object count from a PACK v2 byte stream.
fn pack_header_count(bytes: &[u8]) -> u32 {
    assert_eq!(&bytes[0..4], b"PACK", "must start with PACK magic");
    assert_eq!(
        u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        2
    );
    u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]])
}

struct Fixture {
    dir: tempfile::TempDir,
    c1: ObjectId,
    c2: ObjectId,
    /// Objects introduced by C1: the commit, its tree, and its blob.
    c1_objects: Vec<ObjectId>,
    /// Objects introduced by C2 only: the new commit, the new tree, the new blob.
    c2_new_objects: Vec<ObjectId>,
}

fn build_fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();

    git(dir, &["init", "-q", "-b", "main", "."]);

    // C1: base commit with one file.
    std::fs::write(dir.join("a.txt"), b"hello\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    let c1 = rev_parse(dir, "HEAD");

    let c1_tree = rev_parse(dir, "HEAD^{tree}");
    let c1_blob = rev_parse(dir, "HEAD:a.txt");

    // C2: add a second file (introduces a new commit, a new root tree, a new blob).
    std::fs::write(dir.join("b.txt"), b"world\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
    let c2 = rev_parse(dir, "HEAD");

    let c2_tree = rev_parse(dir, "HEAD^{tree}");
    let c2_blob = rev_parse(dir, "HEAD:b.txt");

    Fixture {
        dir: tmp,
        c1,
        c2,
        c1_objects: vec![c1, c1_tree, c1_blob],
        // a.txt blob is unchanged in C2, so it is NOT a new object.
        c2_new_objects: vec![c2, c2_tree, c2_blob],
    }
}

fn open_odb(dir: &Path) -> Odb {
    let git_dir = dir.join(".git");
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir)
}

#[test]
fn build_pack_selects_only_new_objects() {
    let fx = build_fixture();
    let odb = open_odb(fx.dir.path());

    let pack = build_pack(&odb, &[fx.c2], &[fx.c1], &PackBuildOptions::default()).expect("build");

    // (a) starts with PACK.
    assert_eq!(&pack[0..4], b"PACK");

    // (b) header object count equals ONLY the new objects introduced by C2
    // (new commit + new tree + new blob = 3) — proving minimal selection. The
    // a.txt blob and C1's objects, reachable from the have, are excluded.
    let count = pack_header_count(&pack);
    assert_eq!(
        count,
        fx.c2_new_objects.len() as u32,
        "expected exactly the 3 objects new in C2, got {count}"
    );

    // (c) structurally valid: re-parse with grit-lib's pack reader (this also
    // verifies the trailing checksum) and confirm the resolved object set is
    // exactly the new objects.
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
}

#[test]
fn build_pack_with_empty_haves_packs_full_closure() {
    let fx = build_fixture();
    let odb = open_odb(fx.dir.path());

    let pack = build_pack(&odb, &[fx.c2], &[], &PackBuildOptions::default()).expect("build");

    assert_eq!(&pack[0..4], b"PACK");

    // Full closure from C2: C1, C2, C1's tree, C2's tree, a.txt blob, b.txt blob
    // = 6 distinct objects.
    let map = pack_bytes_to_object_map(&pack, &odb).expect("re-parse pack");
    assert_eq!(
        pack_header_count(&pack) as usize,
        map.len(),
        "header count must match resolved object count"
    );
    assert_eq!(
        map.len(),
        6,
        "full closure of C2 is 6 objects, got {}",
        map.len()
    );

    // Every object from both commits must be present.
    for oid in fx.c1_objects.iter().chain(fx.c2_new_objects.iter()) {
        assert!(map.contains_key(oid), "full pack missing {oid}");
    }
    assert!(map.contains_key(&fx.c1));
    assert!(map.contains_key(&fx.c2));
}

// ---------------------------------------------------------------------------
// Phase 6: delta + thin packs.
// ---------------------------------------------------------------------------

/// A repo with a large file edited across several commits, so successive blob
/// versions share a long common prefix (ideal delta candidates).
struct DeltaFixture {
    dir: tempfile::TempDir,
    /// Commit tips C1..C5 (oldest .. newest).
    tips: Vec<ObjectId>,
}

fn build_delta_fixture() -> DeltaFixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main", "."]);

    // Start with a large body, then append a small line per commit. Each version
    // is a strict prefix-extension of the previous (and shares a long LCP), so a
    // size-sorted prefix/window selector will deltify them.
    let mut body = String::new();
    for i in 0..4000 {
        body.push_str(&format!(
            "line {i:05} lorem ipsum dolor sit amet consectetur\n"
        ));
    }

    let mut tips = Vec::new();
    for rev in 0..5 {
        body.push_str(&format!("--- edit number {rev} appended at the end ---\n"));
        std::fs::write(dir.join("big.txt"), body.as_bytes()).unwrap();
        git(dir, &["add", "big.txt"]);
        git(dir, &["commit", "-q", "-m", &format!("c{rev}")]);
        tips.push(rev_parse(dir, "HEAD"));
    }

    DeltaFixture { dir: tmp, tips }
}

/// Count `REF_DELTA` entries whose base OID is NOT present in this pack (the
/// thin-pack signature), reusing grit-lib's own thin detection where possible.
/// Returns `(ref_delta_external_count, ofs_delta_count, total_objects)`.
fn pack_delta_stats(bytes: &[u8], algo_len: usize) -> (usize, usize, usize) {
    // Minimal pack walker: we only need type codes + the REF_DELTA base oids and
    // the set of in-pack object oids (which we get from the resolved map at the
    // call site, so here we just collect base oids and count types).
    let nr = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let mut pos = 12usize;
    let mut ofs = 0usize;
    let mut ref_bases: Vec<Vec<u8>> = Vec::new();
    // Collected raw object header type for each entry.
    for _ in 0..nr {
        let (type_code, _size, consumed) = read_type_size(&bytes[pos..]);
        pos += consumed;
        match type_code {
            6 => {
                ofs += 1;
                // skip the ofs base distance varint
                while bytes[pos] & 0x80 != 0 {
                    pos += 1;
                }
                pos += 1;
            }
            7 => {
                ref_bases.push(bytes[pos..pos + algo_len].to_vec());
                pos += algo_len;
            }
            _ => {}
        }
        // Skip the zlib stream for this object by decompressing to find its end.
        pos += zlib_consume(&bytes[pos..]);
    }
    (ref_bases.len(), ofs, nr)
}

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

/// Decompress one zlib stream from the front of `b`, returning the number of
/// compressed bytes consumed.
fn zlib_consume(b: &[u8]) -> usize {
    use std::io::Read;
    let mut dec = flate2::bufread::ZlibDecoder::new(b);
    let mut sink = Vec::new();
    dec.read_to_end(&mut sink).expect("zlib decode");
    dec.total_in() as usize
}

#[test]
fn delta_pack_is_smaller_and_reparses() {
    let fx = build_delta_fixture();
    let odb = open_odb(fx.dir.path());
    let tip = *fx.tips.last().unwrap();

    // Whole-object pack (full closure from the tip).
    let whole = build_pack(&odb, &[tip], &[], &PackBuildOptions::default()).expect("whole");
    // Delta pack over the same closure.
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

    // (a) The delta pack must be meaningfully smaller: the five ~200KB blobs
    // collapse to one full blob + four small deltas.
    assert!(
        delta.len() * 2 < whole.len(),
        "delta pack ({}) should be < half the whole pack ({})",
        delta.len(),
        whole.len()
    );

    // (b) Both packs must re-parse to the SAME object set.
    let whole_map = pack_bytes_to_object_map(&whole, &odb).expect("reparse whole");
    let delta_map = pack_bytes_to_object_map(&delta, &odb).expect("reparse delta");
    assert_eq!(
        whole_map.keys().collect::<std::collections::BTreeSet<_>>(),
        delta_map.keys().collect::<std::collections::BTreeSet<_>>(),
        "delta pack must resolve to the same object set as the whole pack"
    );

    // (c) The delta pack must actually contain deltas (OFS or REF).
    let (ref_n, ofs_n, _total) = pack_delta_stats(&delta, 20);
    assert!(
        ref_n + ofs_n >= 3,
        "expected several delta entries, got ref={ref_n} ofs={ofs_n}"
    );

    // (d) System git must accept the delta pack via index-pack.
    assert!(
        git_index_pack_ok(fx.dir.path(), &delta, false),
        "system git index-pack rejected the delta pack"
    );
}

#[test]
fn thin_pack_omits_peer_held_base_and_resolves() {
    let fx = build_delta_fixture();
    let odb = open_odb(fx.dir.path());

    // wants = newest tip, haves = the previous tip (peer already holds C3's
    // closure, including the previous big.txt blob — a perfect external base).
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
        grit_lib::unpack_objects::pack_is_thin(&thin, grit_lib::objects::HashAlgo::Sha1),
        "pack should be detected as thin"
    );

    // (b) At least one REF_DELTA references a base NOT present in the pack.
    let map = pack_bytes_to_object_map(&thin, &odb).expect("thin pack resolves via odb");
    let in_pack: std::collections::HashSet<ObjectId> = map.keys().copied().collect();
    let (ref_n, _ofs_n, _total) = pack_delta_stats(&thin, 20);
    assert!(ref_n >= 1, "thin pack must use at least one REF_DELTA");

    // Re-walk to confirm an external base specifically.
    let external = ref_delta_external_bases(&thin, 20, &in_pack);
    assert!(
        !external.is_empty(),
        "thin pack must reference at least one base NOT in the pack"
    );

    // (c) Supplying the base (it is in our odb) lets every delta resolve: the map
    // above already proves resolution succeeds against the odb-held base. Sanity:
    // the want's tree blob resolves to the newest big.txt content.
    let big_oid = rev_parse(fx.dir.path(), &format!("{}:big.txt", want.to_hex()));
    assert!(
        map.contains_key(&big_oid),
        "resolved thin pack must contain the newest big.txt blob"
    );

    // (d) Cross-check object counts against system git: index-pack --fix-thin in
    // the repo (which holds the base) must accept it and report the same count.
    assert!(
        git_index_pack_ok(fx.dir.path(), &thin, true),
        "system git index-pack --fix-thin rejected the thin pack"
    );
}

/// REF_DELTA base oids that are NOT among `in_pack`.
fn ref_delta_external_bases(
    bytes: &[u8],
    algo_len: usize,
    in_pack: &std::collections::HashSet<ObjectId>,
) -> Vec<ObjectId> {
    let nr = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
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

/// Run system `git index-pack` over `pack` bytes; returns whether it succeeded.
///
/// A self-contained pack is indexed from a file in a scratch dir. A thin pack
/// needs its external bases, so it is fed on stdin inside the fixture repo with
/// `--fix-thin --stdin` (which appends the missing bases and writes a complete
/// pack + idx into the repo's object store).
fn git_index_pack_ok(repo: &Path, pack: &[u8], fix_thin: bool) -> bool {
    use std::io::Write as _;
    use std::process::Stdio;

    if fix_thin {
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
        return out.status.success();
    }

    let scratch = tempfile::tempdir().expect("scratch");
    let pack_path = scratch.path().join("in.pack");
    std::fs::write(&pack_path, pack).unwrap();
    let out = Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", "-v", &pack_path.to_string_lossy()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git index-pack");
    if !out.status.success() {
        eprintln!(
            "git index-pack failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.status.success()
}

// ---------------------------------------------------------------------------
// Deferral 5: delta packing toward CLI parity — islands + on-disk delta reuse.
// ---------------------------------------------------------------------------

/// Index a self-contained (non-thin) pack into a fresh bare repo and run
/// `git fsck --strict` over the resulting object store. Returns whether both the
/// index-pack and the fsck succeeded — i.e. the pack is re-indexable and the
/// objects it carries are well-formed and fully connected internally.
fn git_index_and_fsck_ok(pack: &[u8]) -> bool {
    let scratch = tempfile::tempdir().expect("scratch repo");
    // A bare repo gives index-pack a place to drop the pack + idx, and fsck a
    // store to validate. `--fix-thin` is NOT used: this asserts the pack stands
    // on its own (its bases are all in-pack).
    let init = Command::new("git")
        .current_dir(scratch.path())
        .args(["init", "-q", "--bare", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git init --bare");
    if !init.status.success() {
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

    // fsck the whole store. `--connectivity-only` would skip content checks, so
    // we run the full strict fsck: it parses every object and verifies deltas
    // resolve and hashes match.
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

/// Pack the exact object closure of `tips` with system `git pack-objects` and
/// return the resulting pack's size in bytes — the parity yardstick for our
/// own delta packer. Uses `--delta-base-offset` (OFS deltas, like ours) and the
/// default window/depth.
fn git_pack_objects_size(repo: &Path, tips: &[ObjectId]) -> usize {
    use std::io::Write as _;
    use std::process::Stdio;

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

/// A two-island repo. Branch `a` and branch `b` each carry a `data.txt` whose
/// content shares a long common prefix with the other branch's version (perfect
/// cross-island delta bait), plus per-branch history so islands are non-trivial.
struct IslandFixture {
    dir: tempfile::TempDir,
    tip_a: ObjectId,
    tip_b: ObjectId,
    /// The `data.txt` blob on branch `a` (the larger one — a tempting base).
    blob_a: ObjectId,
    /// The `data.txt` blob on branch `b` (the smaller one — a tempting target).
    blob_b: ObjectId,
}

fn build_island_fixture() -> IslandFixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "a", "."]);

    // Shared prefix body. Branch `b`'s blob is a strict prefix of branch `a`'s
    // (b == prefix, a == prefix + extra), so the size-sorted prefix heuristic
    // would, absent islands, delta the smaller `b` blob against the larger `a`
    // blob — a cross-island delta we want islands to forbid.
    let mut prefix = String::new();
    for i in 0..3000 {
        prefix.push_str(&format!("shared line {i:05} lorem ipsum dolor sit amet\n"));
    }

    // Branch a: data.txt = prefix + a long unique tail (the larger blob).
    let mut a_body = prefix.clone();
    for i in 0..1500 {
        a_body.push_str(&format!("branch-a tail {i:05} aaaaaaaaaaaaaaaaaaaaaaaa\n"));
    }
    std::fs::write(dir.join("data.txt"), a_body.as_bytes()).unwrap();
    git(dir, &["add", "data.txt"]);
    git(dir, &["commit", "-q", "-m", "a"]);
    let tip_a = rev_parse(dir, "HEAD");
    let blob_a = rev_parse(dir, "HEAD:data.txt");

    // Branch b off the root, data.txt = exactly the shared prefix (smaller blob,
    // a strict prefix of a's blob).
    git(dir, &["checkout", "-q", "--orphan", "b"]);
    git(dir, &["rm", "-q", "-f", "--cached", "data.txt"]);
    std::fs::write(dir.join("data.txt"), prefix.as_bytes()).unwrap();
    git(dir, &["add", "data.txt"]);
    git(dir, &["commit", "-q", "-m", "b"]);
    let tip_b = rev_parse(dir, "HEAD");
    let blob_b = rev_parse(dir, "HEAD:data.txt");

    assert_ne!(blob_a, blob_b, "the two branches must have distinct blobs");

    IslandFixture {
        dir: tmp,
        tip_a,
        tip_b,
        blob_a,
        blob_b,
    }
}

#[test]
fn delta_pack_reindexes_fscks_and_is_near_pack_objects_size() {
    let fx = build_delta_fixture();
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

    // (1) Re-indexes via system git index-pack AND passes a strict fsck in a
    // fresh store (self-contained, deltas resolve, hashes verify).
    assert!(
        git_index_and_fsck_ok(&delta),
        "delta pack failed index-pack + fsck --strict"
    );

    // (2a) Meaningfully smaller than whole-object packing.
    assert!(
        delta.len() * 2 < whole.len(),
        "delta pack ({}) should be < half the whole pack ({})",
        delta.len(),
        whole.len()
    );

    // (2b) Within a reasonable factor of `git pack-objects` for the same closure.
    // Our heuristic packer is not expected to match Git byte-for-byte, but it must
    // be in the same ballpark — assert it is no more than 6x the CLI's size (and
    // report the ratio). Git's zdelta + sliding window will usually beat us; the
    // generous bound guards against a pathological regression, not micro-tuning.
    let git_size = git_pack_objects_size(fx.dir.path(), &[tip]);
    assert!(
        delta.len() <= git_size * 6,
        "delta pack ({}) is more than 6x git pack-objects ({}), ratio {:.2}",
        delta.len(),
        git_size,
        delta.len() as f64 / git_size as f64
    );
    eprintln!(
        "delta_pack parity: ours={} git={} ratio={:.2} whole={}",
        delta.len(),
        git_size,
        delta.len() as f64 / git_size as f64,
        whole.len()
    );
}

#[test]
fn islands_forbid_cross_island_blob_delta() {
    let fx = build_island_fixture();
    let dir = fx.dir.path();
    let odb = open_odb(dir);

    // Sanity: blob_b is a strict prefix of blob_a (so the size-sorted prefix
    // heuristic WANTS to delta blob_b against blob_a absent islands).
    let a_data = odb.read(&fx.blob_a).expect("read blob_a").data;
    let b_data = odb.read(&fx.blob_b).expect("read blob_b").data;
    assert!(
        a_data.starts_with(&b_data) && a_data.len() > b_data.len(),
        "fixture invariant: blob_b must be a strict prefix of blob_a"
    );

    let wants = [fx.tip_a, fx.tip_b];

    // (A) WITHOUT islands (respect_islands=false): the cross-blob delta is taken.
    let no_islands = build_pack(
        &odb,
        &wants,
        &[],
        &PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("pack without islands");
    let edges_off = pack_delta_edges_resolved(&no_islands, &odb);
    assert!(
        edges_off
            .iter()
            .any(|(t, b)| *t == fx.blob_b && *b == fx.blob_a),
        "without islands, blob_b should delta against blob_a (got edges {:?})",
        edges_off
    );

    // (B) WITH islands configured so each branch is its own island. Now blob_b's
    // island {b} is NOT a subset of blob_a's island {a}, so the delta is illegal.
    git(dir, &["config", "pack.island", "refs/heads/(.*)"]);
    let islands = build_pack(
        &odb,
        &wants,
        &[],
        &PackBuildOptions {
            delta: true,
            respect_islands: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("pack with islands");

    let edges = pack_delta_edges_resolved(&islands, &odb);
    assert!(
        !edges.iter().any(|(t, b)| *t == fx.blob_b && *b == fx.blob_a),
        "island rule violated: blob_b ({}) was delta'd against cross-island base blob_a ({}); edges {:?}",
        fx.blob_b.to_hex(),
        fx.blob_a.to_hex(),
        edges
    );

    // The island pack must still be valid (re-indexable + fsck-clean).
    assert!(
        git_index_and_fsck_ok(&islands),
        "island delta pack failed index-pack + fsck"
    );

    // And resolve to the same object set as the no-island pack.
    let m1 = pack_bytes_to_object_map(&no_islands, &odb).expect("reparse no-islands");
    let m2 = pack_bytes_to_object_map(&islands, &odb).expect("reparse islands");
    assert_eq!(
        m1.keys().collect::<std::collections::BTreeSet<_>>(),
        m2.keys().collect::<std::collections::BTreeSet<_>>(),
        "island toggle must not change the packed object set"
    );
}

/// Raw per-entry record from a structural walk of a pack: where the entry's
/// header starts, where its (possibly delta) payload zlib stream starts, the
/// pack type code, and the base it points at.
struct RawEntry {
    payload_start: usize,
    type_code: u8,
    base: EdgeBase,
}

enum EdgeBase {
    None,
    /// OFS_DELTA base, by the entry's absolute start offset.
    OfsAt(usize),
    /// REF_DELTA base, by oid (may be external/thin).
    Ref(ObjectId),
}

/// Structurally walk `pack`, returning one [`RawEntry`] per object in pack order.
fn walk_pack_entries(pack: &[u8], algo_len: usize) -> Vec<RawEntry> {
    let nr = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as usize;
    let mut pos = 12usize;
    let mut out = Vec::with_capacity(nr);
    for _ in 0..nr {
        let start = pos;
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
    out
}

/// Resolve every delta edge in `pack` to `(target_oid, base_oid)`.
///
/// Reconstructs each object's content (inflating + applying deltas through the
/// base chain) and Git-hashes it to recover the target oid; OFS bases are
/// resolved via their in-pack offset, REF bases by the named oid (external/thin
/// bases are looked up in `odb`). Returns the `(target, base)` pairs.
fn pack_delta_edges_resolved(pack: &[u8], odb: &Odb) -> Vec<(ObjectId, ObjectId)> {
    use std::collections::HashMap;

    let algo = odb.hash_algo();
    let algo_len = algo.len();
    let entries = walk_pack_entries(pack, algo_len);

    // start_offset -> entry index, so an OFS base (named by absolute start offset)
    // can find its producing entry. Re-walk to recover each entry's start offset.
    let mut start_to_idx: HashMap<usize, usize> = HashMap::new();
    {
        let mut pos = 12usize;
        for (i, _) in entries.iter().enumerate() {
            let start = pos;
            let (type_code, _s, consumed) = read_type_size(&pack[pos..]);
            pos += consumed;
            match type_code {
                6 => {
                    while pack[pos] & 0x80 != 0 {
                        pos += 1;
                    }
                    pos += 1;
                }
                7 => pos += algo_len,
                _ => {}
            }
            pos += zlib_consume(&pack[pos..]);
            start_to_idx.insert(start, i);
        }
    }

    // Memoized reconstruction: content + git type for each entry index.
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
                // In-pack base if present; else external (thin) base from odb.
                let (base_content, base_type) = if let Ok(obj) = odb.read(oid) {
                    let tc = match obj.kind {
                        grit_lib::objects::ObjectKind::Commit => 1,
                        grit_lib::objects::ObjectKind::Tree => 2,
                        grit_lib::objects::ObjectKind::Blob => 3,
                        grit_lib::objects::ObjectKind::Tag => 4,
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

    // Resolve and hash every entry's oid.
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

    // Emit (target, base) edges.
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

/// Inflate a single zlib stream from the front of `b`.
fn inflate(b: &[u8]) -> Vec<u8> {
    use std::io::Read as _;
    let mut dec = flate2::bufread::ZlibDecoder::new(b);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("inflate");
    out
}

/// Git object id of `content` under `kind` ("blob"/"tree"/...) at the given hash.
fn git_hash_object(kind: &str, content: &[u8], algo: grit_lib::objects::HashAlgo) -> ObjectId {
    use grit_lib::hash::hash_object;
    use grit_lib::objects::ObjectKind;
    let kind = match kind {
        "blob" => ObjectKind::Blob,
        "tree" => ObjectKind::Tree,
        "commit" => ObjectKind::Commit,
        "tag" => ObjectKind::Tag,
        other => panic!("unknown object kind {other}"),
    };
    hash_object(algo, kind, content)
}

/// Apply a Git delta instruction stream (copy/insert ops) to `base`.
fn apply_delta(base: &[u8], delta: &[u8]) -> Vec<u8> {
    let mut i = 0usize;
    // skip source size varint
    while delta[i] & 0x80 != 0 {
        i += 1;
    }
    i += 1;
    // skip target size varint
    while delta[i] & 0x80 != 0 {
        i += 1;
    }
    i += 1;
    let mut out = Vec::new();
    while i < delta.len() {
        let op = delta[i];
        i += 1;
        if op & 0x80 != 0 {
            // copy from base
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
            // insert literal
            let len = op as usize;
            out.extend_from_slice(&delta[i..i + len]);
            i += len;
        }
    }
    out
}

#[test]
fn reuse_deltas_produces_valid_pack() {
    // A repo with several similar blobs that git will store as on-disk deltas
    // after a repack; building with reuse_deltas should reuse those edges and
    // still produce a valid (re-indexable, fsck-clean) pack.
    let fx = build_delta_fixture();
    let dir = fx.dir.path();
    // Force git to create on-disk deltas to reuse.
    git(
        dir,
        &[
            "repack",
            "-q",
            "-a",
            "-d",
            "-f",
            "--window=10",
            "--depth=50",
        ],
    );

    let odb = open_odb(dir);
    let tip = *fx.tips.last().unwrap();

    let reuse = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            reuse_deltas: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("reuse pack");

    // Re-indexes + fscks.
    assert!(
        git_index_and_fsck_ok(&reuse),
        "reuse-delta pack failed index-pack + fsck"
    );

    // Resolves to the same object set as a fresh-delta pack.
    let fresh = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("fresh pack");
    let m1 = pack_bytes_to_object_map(&reuse, &odb).expect("reparse reuse");
    let m2 = pack_bytes_to_object_map(&fresh, &odb).expect("reparse fresh");
    assert_eq!(
        m1.keys().collect::<std::collections::BTreeSet<_>>(),
        m2.keys().collect::<std::collections::BTreeSet<_>>(),
        "reuse_deltas must not change the packed object set"
    );

    // And still contains deltas.
    let (ref_n, ofs_n, _t) = pack_delta_stats(&reuse, 20);
    assert!(
        ref_n + ofs_n >= 1,
        "reuse pack should still contain delta entries (ref={ref_n} ofs={ofs_n})"
    );
}

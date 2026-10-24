//! Verbatim pack reuse and streaming upload-pack (Git `index-pack --strict`).

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::pack::packed_full_object_slice;
use grit_lib::objects::pack_objects::{
    write_upload_pack_stream, PackBuildOptions, PackProgress, PackStats,
};
use grit_lib::objects::ObjectId;
use grit_lib::remote::serve::{upload_pack, ProtocolVersion, ServeOptions};
use grit_lib::repo::Repository;
use grit_test_support::{HashAlgo, RepoFixture};

fn git_ok(repo: &RepoFixture, args: &[&str]) {
    let out = repo.git(args);
    assert!(out.ok, "git {:?} failed: {}", args, out.stderr);
}

/// Index `pack` with system `git index-pack --strict`.
///
/// Self-contained clone packs are checked in a fresh bare repo. Incremental and thin
/// packs reference bases the peer already has, so indexing runs inside `have_repo`.
fn git_index_pack_strict(pack: &[u8], algo: HashAlgo, fix_thin: bool, have_repo: Option<&Path>) {
    if fix_thin {
        let repo = have_repo.expect("thin packs need a repo with delta bases");
        let mut child = Command::new("git")
            .current_dir(repo)
            .args(["index-pack", "--strict", "--fix-thin", "--stdin"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn index-pack");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(pack)
            .expect("write pack");
        let out = child.wait_with_output().expect("wait index-pack");
        assert!(
            out.status.success(),
            "git index-pack --strict --fix-thin --stdin: {}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        return;
    }

    if let Some(repo) = have_repo {
        let pack_path = repo.join("grit-test.pack");
        std::fs::write(&pack_path, pack).expect("write pack");
        let out = Command::new("git")
            .current_dir(repo)
            .args(["index-pack", "--strict", "grit-test.pack"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("index-pack");
        let _ = std::fs::remove_file(&pack_path);
        assert!(
            out.status.success(),
            "git index-pack --strict: {}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch");
    let init_args: &[&str] = if algo == HashAlgo::Sha256 {
        &["init", "-q", "--bare", "--object-format=sha256"]
    } else {
        &["init", "-q", "--bare"]
    };
    let init = Command::new("git")
        .current_dir(scratch.path())
        .args(init_args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("init");
    assert!(
        init.status.success(),
        "git init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(scratch.path().join("in.pack"), pack).expect("write");
    let out = Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", "--strict", "in.pack"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        out.status.success(),
        "git index-pack --strict: {}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
}

fn git_fsck(repo: &RepoFixture) {
    let out = repo.git(&["fsck"]);
    assert!(out.ok, "git fsck: {}", out.stderr);
}

fn build_history(repo: &RepoFixture) {
    git_ok(repo, &["checkout", "-b", "main"]);
    std::fs::write(repo.path().join("a.txt"), b"v1").unwrap();
    git_ok(repo, &["add", "a.txt"]);
    git_ok(repo, &["commit", "-m", "c1"]);
    std::fs::write(
        repo.path().join("a.txt"),
        b"v2 longer payload for delta reuse",
    )
    .unwrap();
    git_ok(repo, &["commit", "-am", "c2"]);
    git_ok(repo, &["repack", "-adb"]);
}

fn stream_clone_pack(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    opts: &PackBuildOptions,
) -> (Vec<u8>, PackStats) {
    let mut buf = Vec::new();
    let mut progress = |_ev: PackProgress| {};
    let stats = write_upload_pack_stream(
        repo,
        wants,
        haves,
        &HashSet::new(),
        &HashSet::new(),
        None,
        opts,
        &mut progress,
        &mut buf,
    )
    .expect("stream pack");
    (buf, stats)
}

fn assert_verbatim_blob_in_pack(objects: &Path, pack: &[u8], blob: &ObjectId) {
    let verbatim = packed_full_object_slice(objects, blob)
        .expect("lookup")
        .expect("reusable slice");
    assert!(
        pack.windows(verbatim.len())
            .any(|w| w == verbatim.as_slice()),
        "pack must contain verbatim reused bytes for {blob}"
    );
}

#[test]
fn full_clone_reuses_pack_and_git_index_pack_strict_ok() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let opts = PackBuildOptions {
        delta: true,
        use_bitmaps: true,
        use_ofs_delta: true,
        thin: false,
        ..PackBuildOptions::default()
    };
    let (pack, stats) = stream_clone_pack(&repo, &[tip], &[], &opts);
    git_index_pack_strict(&pack, HashAlgo::Sha1, false, None);
    assert!(
        stats.verbatim_reuse_bytes > 0,
        "expected verbatim reuse: {stats:?}"
    );
    let objects = fx.path().join(".git").join("objects");
    let blob = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD:a.txt"]).stdout.trim()).unwrap();
    assert_verbatim_blob_in_pack(&objects, &pack, &blob);
}

#[test]
fn partial_reuse_with_gaps_rewrites_ofs_delta() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let parent = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD~1"]).stdout.trim()).unwrap();
    let opts = PackBuildOptions {
        delta: true,
        use_bitmaps: true,
        use_ofs_delta: true,
        thin: false,
        ..PackBuildOptions::default()
    };
    let (pack, stats) = stream_clone_pack(&repo, &[tip], &[parent], &opts);
    git_index_pack_strict(&pack, HashAlgo::Sha1, false, Some(fx.path()));
    git_fsck(&fx);
    assert!(
        stats.verbatim_reuse_bytes > 0 || stats.pack_ofs_fixup_objects > 0,
        "expected reuse or OFS rewrite: {stats:?}"
    );
}

#[test]
fn midx_preferred_pack_reuse() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    git_ok(&fx, &["repack", "-adb"]);
    std::fs::write(fx.path().join("b.txt"), b"after repack").unwrap();
    git_ok(&fx, &["add", "b.txt"]);
    git_ok(&fx, &["commit", "-m", "loose after repack"]);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let opts = PackBuildOptions {
        delta: true,
        use_bitmaps: true,
        ..PackBuildOptions::default()
    };
    let (pack, stats) = stream_clone_pack(&repo, &[tip], &[], &opts);
    git_index_pack_strict(&pack, HashAlgo::Sha1, false, None);
    assert!(
        stats.verbatim_reuse_bytes > 0,
        "expected pack reuse: {stats:?}"
    );
}

#[test]
fn extended_objects_appended() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    std::fs::write(fx.path().join("loose.txt"), b"loose-only").unwrap();
    git_ok(&fx, &["add", "loose.txt"]);
    git_ok(&fx, &["commit", "-m", "add loose"]);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let opts = PackBuildOptions {
        delta: true,
        use_bitmaps: true,
        ..PackBuildOptions::default()
    };
    let (pack, stats) = stream_clone_pack(&repo, &[tip], &[], &opts);
    git_index_pack_strict(&pack, HashAlgo::Sha1, false, None);
    assert!(
        stats.verbatim_reuse_bytes > 0,
        "expected reuse of packed history: {stats:?}"
    );
    assert!(
        stats.pack_bytes > stats.verbatim_reuse_bytes,
        "extended loose objects must add newly encoded bytes beyond reuse: {stats:?}"
    );
}

#[test]
fn sha256_reuse() {
    let fx = RepoFixture::init(HashAlgo::Sha256).expect("init");
    git_ok(&fx, &["checkout", "-b", "main"]);
    std::fs::write(fx.path().join("f"), b"sha256 reuse test v1").unwrap();
    git_ok(&fx, &["add", "f"]);
    git_ok(&fx, &["commit", "-m", "c1"]);
    std::fs::write(
        fx.path().join("f"),
        b"sha256 reuse test v2 longer for delta chain",
    )
    .unwrap();
    git_ok(&fx, &["add", "f"]);
    git_ok(&fx, &["commit", "-m", "c2"]);
    git_ok(&fx, &["repack", "-adb"]);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let parent = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD~1"]).stdout.trim()).unwrap();
    let opts = PackBuildOptions {
        delta: true,
        use_bitmaps: true,
        thin: true,
        use_ofs_delta: false,
        ..PackBuildOptions::default()
    };
    let (pack, stats) = stream_clone_pack(&repo, &[tip], &[parent], &opts);
    git_index_pack_strict(&pack, HashAlgo::Sha256, true, Some(fx.path()));
    assert!(
        stats.verbatim_reuse_bytes > 0 || stats.pack_ref_from_ofs_objects > 0,
        "expected sha256 reuse or thin REF conversion: {stats:?}"
    );
    let objects = git_dir.join("objects");
    let blob = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD:f"]).stdout.trim()).unwrap();
    assert_verbatim_blob_in_pack(&objects, &pack, &blob);
}

#[test]
fn bitmap_pack_with_haves_git_index_pack_strict() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    git_ok(&fx, &["repack", "-adb"]);
    std::fs::write(fx.path().join("post.txt"), b"post-bitmap loose").unwrap();
    git_ok(&fx, &["add", "post.txt"]);
    git_ok(&fx, &["commit", "-m", "post bitmap"]);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let have = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD~2"]).stdout.trim()).unwrap();
    let opts = PackBuildOptions {
        delta: true,
        use_bitmaps: true,
        use_ofs_delta: true,
        thin: false,
        ..PackBuildOptions::default()
    };
    let (pack, stats) = stream_clone_pack(&repo, &[tip], &[have], &opts);
    git_index_pack_strict(&pack, HashAlgo::Sha1, false, Some(fx.path()));
    git_fsck(&fx);
    assert!(
        stats.verbatim_reuse_bytes > 0 || stats.pack_ofs_fixup_objects > 0,
        "expected bitmap reuse with haves: {stats:?}"
    );
}

#[test]
fn reuse_skips_when_have_shallow_grafts_present() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    git_ok(&fx, &["repack", "-adb"]);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let parent = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD~1"]).stdout.trim()).unwrap();
    let mut have_shallow = HashSet::new();
    have_shallow.insert(parent);
    let opts = PackBuildOptions {
        delta: true,
        use_bitmaps: true,
        ..PackBuildOptions::default()
    };
    let mut buf = Vec::new();
    let mut progress = |_ev: PackProgress| {};
    let out = grit_lib::objects::pack::try_write_streaming_pack(
        &repo,
        &[tip],
        &[parent],
        &HashSet::new(),
        &have_shallow,
        None,
        &opts,
        &mut progress,
        &mut buf,
    )
    .expect("try reuse");
    assert!(
        out.is_none(),
        "bitmap reuse must fall back when client shallow grafts are present"
    );
}

#[test]
fn thin_pack_bitmap_reuse_index_pack_strict_ok() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    git_ok(&fx, &["repack", "-adb"]);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let parent = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD~1"]).stdout.trim()).unwrap();
    let opts = PackBuildOptions {
        delta: true,
        thin: true,
        use_ofs_delta: true,
        use_bitmaps: true,
        ..PackBuildOptions::default()
    };
    let mut buf = Vec::new();
    let mut progress = |_ev: PackProgress| {};
    write_upload_pack_stream(
        &repo,
        &[tip],
        &[parent],
        &HashSet::new(),
        &HashSet::new(),
        None,
        &opts,
        &mut progress,
        &mut buf,
    )
    .expect("thin pack with bitmap ofs reuse");
    git_index_pack_strict(&buf, HashAlgo::Sha1, true, Some(fx.path()));
}

#[test]
fn multi_sidecar_pack_reuse_binds_bitmap_backing_pack() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    git_ok(&fx, &["checkout", "-b", "main"]);
    std::fs::write(fx.path().join("seed.txt"), b"seed").unwrap();
    git_ok(&fx, &["add", "seed.txt"]);
    git_ok(&fx, &["commit", "-m", "seed"]);
    git_ok(&fx, &["repack", "-adb"]);
    let bitmapped_tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();

    for i in 0..100 {
        std::fs::write(fx.path().join(format!("bulk-{i}.txt")), format!("data-{i}")).unwrap();
        git_ok(&fx, &["add", &format!("bulk-{i}.txt")]);
        git_ok(&fx, &["commit", "-m", &format!("bulk {i}")]);
    }
    git_ok(
        &fx,
        &[
            "pack-objects",
            "--all",
            "--unpacked",
            &fx.path().join(".git/objects/pack/large").to_string_lossy(),
        ],
    );

    let pack_dir = fx.path().join(".git/objects/pack");
    let bitmap_count = std::fs::read_dir(&pack_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "bitmap"))
        .count();
    assert_eq!(
        bitmap_count, 1,
        "expected one pack bitmap sidecar with a larger pack without bitmap"
    );

    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let opts = PackBuildOptions {
        delta: true,
        use_bitmaps: true,
        use_ofs_delta: true,
        thin: false,
        ..PackBuildOptions::default()
    };
    let (pack, stats) = stream_clone_pack(&repo, &[bitmapped_tip], &[], &opts);
    git_index_pack_strict(&pack, HashAlgo::Sha1, false, None);
    assert!(
        stats.verbatim_reuse_bytes > 0,
        "reuse must bind the bitmap sidecar pack, not the larger pack: {stats:?}"
    );
    let objects = git_dir.join("objects");
    let blob = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD:seed.txt"]).stdout.trim()).unwrap();
    assert_verbatim_blob_in_pack(&objects, &pack, &blob);
}

#[test]
fn thin_pack_ref_delta_conversion() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let parent = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD~1"]).stdout.trim()).unwrap();
    let opts = PackBuildOptions {
        delta: true,
        thin: true,
        use_ofs_delta: false,
        use_bitmaps: true,
        ..PackBuildOptions::default()
    };
    let mut buf = Vec::new();
    let mut progress = |_ev: PackProgress| {};
    let stats = write_upload_pack_stream(
        &repo,
        &[tip],
        &[parent],
        &HashSet::new(),
        &HashSet::new(),
        None,
        &opts,
        &mut progress,
        &mut buf,
    )
    .expect("thin pack");
    git_index_pack_strict(&buf, HashAlgo::Sha1, true, Some(fx.path()));
    assert!(
        stats.pack_ref_from_ofs_objects > 0 || stats.verbatim_reuse_bytes > 0,
        "expected thin REF conversion or delta reuse: {stats:?}"
    );
}

/// Deterministic incompressible bytes, so pack sizes do not depend on zlib.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// History with large distinct blobs and a growing text file whose revisions git
/// stores as deltas, packed by git into one pack without a reachability bitmap.
fn build_packed_history_without_bitmap(repo: &RepoFixture) -> ObjectId {
    git_ok(repo, &["config", "gc.auto", "0"]);
    git_ok(repo, &["checkout", "-q", "-b", "main"]);
    let mut log = String::new();
    for i in 0..24u64 {
        log.push_str(&format!("line {i}: {}\n", "x".repeat(200)));
        std::fs::write(repo.path().join("log.txt"), &log).unwrap();
        if i % 3 == 0 {
            std::fs::write(
                repo.path().join(format!("big-{i}.bin")),
                noise(i, 96 * 1024),
            )
            .unwrap();
        }
        git_ok(repo, &["add", "-A"]);
        git_ok(repo, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git_ok(
        repo,
        &["-c", "repack.writeBitmaps=false", "repack", "-adf", "-q"],
    );
    let pack_dir = repo.path().join(".git/objects/pack");
    let files: Vec<_> = std::fs::read_dir(&pack_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    assert_eq!(
        files
            .iter()
            .filter(|p| p.extension().is_some_and(|x| x == "pack"))
            .count(),
        1
    );
    assert!(!files
        .iter()
        .any(|p| p.extension().is_some_and(|x| x == "bitmap")));
    ObjectId::from_hex(repo.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap()
}

/// Records every `write` call so tests can tell a streamed pack from a buffered one.
#[derive(Default)]
struct RecordingWriter {
    bytes: Vec<u8>,
    writes: usize,
    largest_write: usize,
}

impl Write for RecordingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        self.largest_write = self.largest_write.max(buf.len());
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Index `pack` into a fresh bare repository with `git index-pack --strict`, point
/// `refs/heads/main` at `tip`, and require `git fsck --strict` to pass.
///
/// Returns the pack type code of every entry (`6` = `OFS_DELTA`, `7` = `REF_DELTA`).
fn index_and_fsck_clone(pack: &[u8], tip: &ObjectId) -> Vec<u8> {
    let scratch = tempfile::tempdir().expect("scratch");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(scratch.path())
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q", "--bare"]);
    let pack_path = scratch.path().join("objects/pack/pack-in.pack");
    std::fs::write(&pack_path, pack).expect("write pack");
    git(&["index-pack", "--strict", "objects/pack/pack-in.pack"]);
    git(&["update-ref", "refs/heads/main", &tip.to_hex()]);
    git(&["fsck", "--strict", "--no-dangling"]);
    let idx = std::fs::File::open(scratch.path().join("objects/pack/pack-in.idx")).expect("idx");
    let listing = Command::new("git")
        .current_dir(scratch.path())
        .args(["show-index"])
        .stdin(Stdio::from(idx))
        .output()
        .expect("show-index");
    assert!(
        listing.status.success(),
        "git show-index: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(|line| {
            let offset: usize = line.split(' ').next().unwrap().parse().unwrap();
            (pack[offset] >> 4) & 0x7
        })
        .collect()
}

/// Concatenate side-band channel 1 from a pkt-line response, skipping
/// non-band lines (`NAK`, `packfile`) and progress.
fn sideband_pack(response: &[u8]) -> Vec<u8> {
    let mut pack = Vec::new();
    let mut rest = response;
    while rest.len() >= 4 {
        let len = usize::from_str_radix(std::str::from_utf8(&rest[..4]).unwrap(), 16).unwrap();
        if len < 4 {
            rest = &rest[4..];
            continue;
        }
        let payload = &rest[4..len];
        match payload.first() {
            Some(1) => pack.extend_from_slice(&payload[1..]),
            Some(3) => panic!("server error: {}", String::from_utf8_lossy(&payload[1..])),
            _ => {}
        }
        rest = &rest[len..];
    }
    pack
}

fn upload_pack_full_clone(repo: &Repository, tip: &ObjectId, protocol: ProtocolVersion) -> Vec<u8> {
    let pkt = |s: &str| format!("{:04x}{s}", s.len() + 4).into_bytes();
    let mut body = Vec::new();
    match protocol {
        ProtocolVersion::V2 => {
            body.extend(pkt("command=fetch\n"));
            body.extend_from_slice(b"0001");
            for arg in ["thin-pack", "ofs-delta", "no-progress"] {
                body.extend(pkt(&format!("{arg}\n")));
            }
            body.extend(pkt(&format!("want {}\n", tip.to_hex())));
            body.extend(pkt("done\n"));
            body.extend_from_slice(b"0000");
        }
        _ => {
            let caps = "multi_ack_detailed side-band-64k thin-pack no-progress ofs-delta";
            body.extend(pkt(&format!("want {} {caps}\n", tip.to_hex())));
            body.extend_from_slice(b"0000");
            body.extend(pkt("done\n"));
        }
    }
    let mut out = Vec::new();
    upload_pack(
        repo,
        &mut std::io::Cursor::new(body),
        &mut out,
        &ServeOptions {
            protocol,
            advertise_refs: false,
            stateless_rpc: true,
            agent: "grit-test".to_owned(),
            hidden_refs: Vec::new(),
        },
    )
    .expect("upload-pack");
    sideband_pack(&out)
}

#[test]
fn full_clone_without_bitmap_streams_verbatim_pack_entries() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    let tip = build_packed_history_without_bitmap(&fx);
    let repo = Repository::open(&fx.path().join(".git"), &RepositoryOptions::empty()).unwrap();
    let opts = PackBuildOptions {
        thin: true,
        delta: true,
        use_ofs_delta: true,
        ..PackBuildOptions::default()
    };
    let mut writer = RecordingWriter::default();
    let mut progress = |_ev: PackProgress| {};
    let stats = write_upload_pack_stream(
        &repo,
        &[tip],
        &[],
        &HashSet::new(),
        &HashSet::new(),
        None,
        &opts,
        &mut progress,
        &mut writer,
    )
    .expect("stream pack");

    let expected_objects = fx
        .git(&["rev-list", "--objects", "--all"])
        .stdout
        .lines()
        .count();
    assert_eq!(stats.object_count as usize, expected_objects);
    assert!(stats.delta_count > 0, "git stored deltas: {stats:?}");
    assert_eq!(
        stats.verbatim_reuse_objects, stats.object_count,
        "every entry of an already-packed repository is copied from its pack: {stats:?}"
    );
    assert_eq!(stats.pack_bytes, writer.bytes.len() as u64);
    assert!(
        writer.writes > 16 && writer.largest_write < writer.bytes.len() / 4,
        "pack must reach the writer incrementally, not as one buffer: {} writes, largest {} of {}",
        writer.writes,
        writer.largest_write,
        writer.bytes.len()
    );

    let types = index_and_fsck_clone(&writer.bytes, &tip);
    assert_eq!(types.len(), expected_objects);
    assert!(
        !types.contains(&7),
        "delta bases are written first, so every delta is OFS_DELTA"
    );
    let source_pack: u64 = std::fs::read_dir(fx.path().join(".git/objects/pack"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .map(|p| std::fs::metadata(p).unwrap().len())
        .sum();
    assert!(
        stats.pack_bytes <= source_pack + source_pack / 100,
        "reused pack ({}) should be about the size of git's ({source_pack})",
        stats.pack_bytes
    );
}

#[test]
fn upload_pack_full_clone_of_packed_repo_round_trips_through_git() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    let tip = build_packed_history_without_bitmap(&fx);
    let repo = Repository::open(&fx.path().join(".git"), &RepositoryOptions::empty()).unwrap();
    let expected_objects = fx
        .git(&["rev-list", "--objects", "--all"])
        .stdout
        .lines()
        .count();
    for protocol in [ProtocolVersion::V0, ProtocolVersion::V2] {
        let pack = upload_pack_full_clone(&repo, &tip, protocol);
        let types = index_and_fsck_clone(&pack, &tip);
        assert_eq!(types.len(), expected_objects, "{protocol:?}");
        assert!(
            types.contains(&6),
            "{protocol:?}: deltas reused as OFS_DELTA"
        );
    }
}

#[test]
fn upload_pack_honors_pack_window_config() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    git_ok(&fx, &["config", "gc.auto", "0"]);
    git_ok(&fx, &["checkout", "-q", "-b", "main"]);
    let mut text = String::new();
    for i in 0..12 {
        text.push_str(&format!("revision {i} {}\n", "y".repeat(400)));
        std::fs::write(fx.path().join("doc.txt"), &text).unwrap();
        git_ok(&fx, &["commit", "-q", "-a", "--allow-empty", "-m", "r"]);
        if i == 0 {
            git_ok(&fx, &["add", "doc.txt"]);
            git_ok(&fx, &["commit", "-q", "-m", "add"]);
        }
    }
    let tip = ObjectId::from_hex(fx.git(&["rev-parse", "HEAD"]).stdout.trim()).unwrap();
    let delta_entries = |pack: &[u8]| {
        index_and_fsck_clone(pack, &tip)
            .into_iter()
            .filter(|t| *t == 6 || *t == 7)
            .count()
    };

    let repo = Repository::open(&fx.path().join(".git"), &RepositoryOptions::empty()).unwrap();
    let with_window = upload_pack_full_clone(&repo, &tip, ProtocolVersion::V2);
    assert!(
        delta_entries(&with_window) > 0,
        "default window finds deltas"
    );

    git_ok(&fx, &["config", "pack.window", "0"]);
    let repo = Repository::open(&fx.path().join(".git"), &RepositoryOptions::empty()).unwrap();
    let without_window = upload_pack_full_clone(&repo, &tip, ProtocolVersion::V2);
    assert_eq!(
        delta_entries(&without_window),
        0,
        "pack.window=0 disables delta search"
    );
}

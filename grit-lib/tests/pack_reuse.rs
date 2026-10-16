//! Verbatim pack reuse and streaming upload-pack (Git `index-pack --strict`).

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use grit_lib::objects::ObjectId;
use grit_lib::pack::packed_full_object_slice;
use grit_lib::pack_objects::{write_upload_pack_stream, PackBuildOptions, PackProgress, PackStats};
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
    let repo = Repository::open(&git_dir, None).expect("open");
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
    let repo = Repository::open(&git_dir, None).expect("open");
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
    let repo = Repository::open(&git_dir, None).expect("open");
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
    let repo = Repository::open(&git_dir, None).expect("open");
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
    let repo = Repository::open(&git_dir, None).expect("open");
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
    let repo = Repository::open(&git_dir, None).expect("open");
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
fn thin_pack_ref_delta_conversion() {
    let fx = RepoFixture::init(HashAlgo::Sha1).expect("init");
    build_history(&fx);
    let git_dir = fx.path().join(".git");
    let repo = Repository::open(&git_dir, None).expect("open");
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

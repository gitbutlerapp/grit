//! Git compatibility for [`grit_lib::objects::pack_objects`] packs: `index-pack --strict`, `fsck`,
//! thin-pack fix, size parity vs `git pack-objects --revs`, and SHA-256 repos.

use grit_test_support::rev_parse;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use std::collections::HashSet;

use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack_object_select::{
    enumerate_pack_objects, HaveBoundary, PackEnumerateOptions,
};
use grit_lib::objects::pack_objects::{
    build_pack_for_push, PackBuildOptions, PackObjects, PackObjectsOptions,
};
use grit_lib::objects::unpack_objects::pack_bytes_to_object_map;
use grit_lib::objects::ObjectId;
use grit_lib::remote::transfer::build_pack;

const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "T"),
    ("GIT_AUTHOR_EMAIL", "t@example.com"),
    ("GIT_COMMITTER_NAME", "T"),
    ("GIT_COMMITTER_EMAIL", "t@example.com"),
];

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in GIT_ENV {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn open_odb(dir: &Path) -> Odb {
    let git_dir = dir.join(".git");
    Odb::new(git_dir.join("objects").as_path()).with_config_git_dir(git_dir)
}

fn grit_delta_pack(odb: &Odb, wants: &[ObjectId], haves: &[ObjectId]) -> Vec<u8> {
    build_pack(
        odb,
        wants,
        haves,
        &PackBuildOptions {
            delta: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("pack")
}

fn git_pack_objects_with_thin(
    repo: &Path,
    tips: &[ObjectId],
    haves: &[ObjectId],
    thin: bool,
) -> Vec<u8> {
    let mut args = vec!["pack-objects", "--stdout", "--revs", "--delta-base-offset"];
    if thin {
        args.push("--thin");
    }
    let mut child = Command::new("git");
    child
        .current_dir(repo)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in GIT_ENV {
        child.env(k, v);
    }
    let mut child = child.spawn().expect("spawn");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        for t in tips {
            writeln!(stdin, "{}", t.to_hex()).unwrap();
        }
        for h in haves {
            writeln!(stdin, "^{}", h.to_hex()).unwrap();
        }
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git pack-objects: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git_pack_objects(repo: &Path, tips: &[ObjectId], haves: &[ObjectId]) -> Vec<u8> {
    git_pack_objects_with_thin(repo, tips, haves, false)
}

fn pack_object_set(pack: &[u8], odb: &Odb) -> HashSet<ObjectId> {
    pack_bytes_to_object_map(pack, odb)
        .expect("resolve pack")
        .into_keys()
        .collect()
}

/// Complete a thin pack in a fresh repository that holds only `have`'s history from `src`.
///
/// Returns how many external bases `index-pack --fix-thin` appended to the pack.
fn git_index_pack_stdin_fix_thin(src: &Path, have: ObjectId, pack: &[u8]) -> usize {
    let receiver = tempfile::tempdir().expect("receiver");
    git(src, &["branch", "-f", "thin-have", &have.to_hex()]);
    git(receiver.path(), &["init", "-q", "--bare", "."]);
    git(
        receiver.path(),
        &[
            "fetch",
            "-q",
            src.to_str().unwrap(),
            "refs/heads/thin-have:refs/heads/have",
        ],
    );
    let in_pack_before = in_pack_count(receiver.path());
    let mut child = Command::new("git")
        .current_dir(receiver.path())
        .args(["index-pack", "--fix-thin", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("spawn");
    child.stdin.take().unwrap().write_all(pack).unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "index-pack --fix-thin: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(receiver.path(), &["fsck", "--strict"]);
    let header_count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as usize;
    in_pack_count(receiver.path()) - in_pack_before - header_count
}

fn in_pack_count(repo: &Path) -> usize {
    git(repo, &["count-objects", "-v"])
        .lines()
        .find_map(|l| l.strip_prefix("in-pack: "))
        .and_then(|n| n.trim().parse().ok())
        .expect("in-pack count")
}

fn git_index_pack_strict(_repo: &Path, pack: &[u8]) {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let init = Command::new("git")
        .current_dir(tmp.path())
        .args(["init", "-q", "--bare", "."])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("init");
    assert!(init.status.success());
    let pack_path = tmp.path().join("in.pack");
    std::fs::write(&pack_path, pack).unwrap();
    let idx = Command::new("git")
        .current_dir(tmp.path())
        .args(["index-pack", &pack_path.to_string_lossy()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        idx.status.success(),
        "index-pack: {}",
        String::from_utf8_lossy(&idx.stderr)
    );
    let fsck = Command::new("git")
        .current_dir(tmp.path())
        .args(["fsck", "--strict", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

fn build_deep_history_repo(commits: usize) -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main", "."]);
    for i in 0..commits {
        std::fs::write(dir.join(format!("f{i:04}.txt")), format!("body {i}\n")).unwrap();
        std::fs::write(dir.join("shared.txt"), format!("shared {i}\n")).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(dir, &["repack", "-adf", "--depth=50", "-q"]);
    let tip = rev_parse(dir, "HEAD");
    let have = rev_parse(dir, &format!("HEAD~{}", commits.saturating_sub(2).max(1)));
    (tmp, tip, have)
}

/// Divergent tips after long shared history: send only the want-side tip objects.
#[test]
fn incremental_thin_pack_divergent_tips_matches_git_send_size() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main", "."]);
    for i in 0..300 {
        std::fs::write(dir.join(format!("f{i:04}.txt")), format!("shared {i}\n")).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    let base = rev_parse(dir, "HEAD");
    git(dir, &["branch", "have-tip", &base.to_hex()]);
    std::fs::write(dir.join("want.txt"), b"want branch\n").unwrap();
    git(dir, &["checkout", "-q", "-b", "want-tip"]);
    git(dir, &["add", "want.txt"]);
    git(dir, &["commit", "-q", "-m", "want side"]);
    let want = rev_parse(dir, "HEAD");
    git(dir, &["checkout", "-q", "have-tip"]);
    std::fs::write(dir.join("have.txt"), b"have branch\n").unwrap();
    git(dir, &["add", "have.txt"]);
    git(dir, &["commit", "-q", "-m", "have side"]);
    let have = rev_parse(dir, "HEAD");
    git(dir, &["repack", "-adf", "-q"]);

    let odb = open_odb(dir);
    let empty = HashSet::new();
    let selection = enumerate_pack_objects(
        &odb,
        &[want],
        &[have],
        &PackEnumerateOptions {
            filter: None,
            shallow_grafts: &empty,
            have_shallow_grafts: Some(&empty),
            use_bitmaps: false,
            exclude_objects: None,
            peer_odb: None,
            have_boundary: grit_lib::objects::pack_object_select::HaveBoundary::CommitClosure,
        },
    )
    .expect("enumerate");
    assert!(
        selection.send.len() <= 6,
        "divergent tips should send only the want-side tip closure, got {} objects",
        selection.send.len()
    );

    let grit_pack = build_pack(
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
    let git_pack = git_pack_objects(dir, &[want], &[have]);
    assert!(
        grit_pack.len() <= git_pack.len() * 2,
        "grit thin pack {} bytes vs git {} bytes",
        grit_pack.len(),
        git_pack.len()
    );
    git_index_pack_stdin_fix_thin(dir, have, &grit_pack);

    let push_selection = enumerate_pack_objects(
        &odb,
        &[want],
        &[have],
        &PackEnumerateOptions {
            filter: None,
            shallow_grafts: &empty,
            have_shallow_grafts: Some(&empty),
            use_bitmaps: false,
            exclude_objects: None,
            peer_odb: None,
            have_boundary: HaveBoundary::TipsOnly,
        },
    )
    .expect("push enumerate");
    assert!(
        push_selection.send.len() <= 6,
        "send-pack boundary paint should match closure on divergent tips, got {} objects",
        push_selection.send.len()
    );

    let grit_push = build_pack_for_push(
        &odb,
        &[want],
        &[have],
        &PackBuildOptions {
            delta: true,
            thin: true,
            use_bitmaps: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("push pack");
    let git_push = git_pack_objects_with_thin(dir, &[want], &[have], true);
    assert_eq!(
        pack_object_set(&grit_push, &odb),
        pack_object_set(&git_push, &odb),
        "send-pack object set must match git pack-objects --revs --thin"
    );
}

/// Shared history below a have tip (not through the tip commit) must stay excluded.
#[test]
fn send_pack_revs_matches_git_on_merge_history() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("base.txt"), b"base\n").unwrap();
    git(dir, &["add", "base.txt"]);
    git(dir, &["commit", "-q", "-m", "base"]);
    let base = rev_parse(dir, "HEAD");
    git(dir, &["branch", "side", &base.to_hex()]);
    git(dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join("main.txt"), b"main\n").unwrap();
    git(dir, &["add", "main.txt"]);
    git(dir, &["commit", "-q", "-m", "main line"]);
    let main_tip = rev_parse(dir, "HEAD");
    git(dir, &["checkout", "-q", "side"]);
    std::fs::write(dir.join("side.txt"), b"side\n").unwrap();
    git(dir, &["add", "side.txt"]);
    git(dir, &["commit", "-q", "-m", "side line"]);
    let _side_tip = rev_parse(dir, "HEAD");
    git(dir, &["checkout", "-q", "main"]);
    git(dir, &["merge", "-q", "--no-ff", "side", "-m", "merge side"]);
    let want = rev_parse(dir, "HEAD");
    let have = main_tip;
    git(dir, &["repack", "-adf", "-q"]);

    let odb = open_odb(dir);
    let grit_pack = build_pack_for_push(
        &odb,
        &[want],
        &[have],
        &PackBuildOptions {
            delta: true,
            thin: true,
            use_bitmaps: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("grit push pack");
    let git_pack = git_pack_objects_with_thin(dir, &[want], &[have], true);
    assert_eq!(
        pack_object_set(&grit_pack, &odb),
        pack_object_set(&git_pack, &odb),
        "merge push must match git --revs --thin object set"
    );
}

/// Annotated tags on non-commits in the have list are ignored (push must not fail).
#[test]
fn send_pack_ignores_blob_tag_haves() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("f.txt"), b"v1\n").unwrap();
    git(dir, &["add", "f.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    let have = rev_parse(dir, "HEAD");
    std::fs::write(dir.join("f.txt"), b"v2\n").unwrap();
    git(dir, &["commit", "-q", "-am", "c2"]);
    let want = rev_parse(dir, "HEAD");
    git(
        dir,
        &[
            "tag",
            "-a",
            "blob-tag",
            "-m",
            "points at blob",
            "HEAD~1:f.txt",
        ],
    );
    let blob_tag = rev_parse(dir, "refs/tags/blob-tag");
    let odb = open_odb(dir);
    build_pack_for_push(
        &odb,
        &[want],
        &[have, blob_tag],
        &PackBuildOptions {
            delta: true,
            thin: true,
            use_bitmaps: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("push with blob tag have");
}

/// Issue #1975: send-pack must not pre-walk the full commit closure of every advertised ref.
#[test]
fn push_send_pack_uses_tips_only_have_boundary() {
    let (dir, tip, _) = build_deep_history_repo(800);
    let parent = rev_parse(dir.path(), "HEAD~1");
    let mut haves = vec![parent];
    for n in (2..400).step_by(2) {
        haves.push(rev_parse(dir.path(), &format!("HEAD~{n}")));
    }
    let odb = open_odb(dir.path());
    let empty = HashSet::new();
    // Many advertised tips on the same branch; enumeration must stay incremental.
    let selection = enumerate_pack_objects(
        &odb,
        &[tip],
        &haves,
        &PackEnumerateOptions {
            filter: None,
            shallow_grafts: &empty,
            have_shallow_grafts: Some(&empty),
            use_bitmaps: false,
            exclude_objects: None,
            peer_odb: None,
            have_boundary: HaveBoundary::TipsOnly,
        },
    )
    .expect("enumerate");
    assert!(
        selection.send.len() < 40,
        "one new commit with many advertised tips should stay small, got {} objects",
        selection.send.len()
    );
    build_pack_for_push(
        &odb,
        &[tip],
        &haves,
        &PackBuildOptions {
            delta: true,
            thin: true,
            use_bitmaps: false,
            ..PackBuildOptions::default()
        },
    )
    .expect("push pack");
}

/// Issue #1938: incremental enumeration must not walk or load the full peer closure.
#[test]
fn incremental_thin_pack_send_list_is_bounded() {
    let (dir, tip, _) = build_deep_history_repo(120);
    let have = rev_parse(dir.path(), "HEAD~1");
    let odb = open_odb(dir.path());
    let empty = HashSet::new();
    let selection = enumerate_pack_objects(
        &odb,
        &[tip],
        &[have],
        &PackEnumerateOptions {
            filter: None,
            shallow_grafts: &empty,
            have_shallow_grafts: Some(&empty),
            use_bitmaps: true,
            exclude_objects: None,
            peer_odb: None,
            have_boundary: grit_lib::objects::pack_object_select::HaveBoundary::CommitClosure,
        },
    )
    .expect("enumerate");
    assert!(
        selection.send.len() < 40,
        "expected a small send set for one commit behind, got {} objects",
        selection.send.len()
    );
    let thin = build_pack(
        &odb,
        &[tip],
        &[have],
        &PackBuildOptions {
            delta: true,
            thin: true,
            ..PackBuildOptions::default()
        },
    )
    .expect("thin pack");
    git_index_pack_stdin_fix_thin(dir.path(), have, &thin);
    let map = pack_bytes_to_object_map(&thin, &odb).expect("resolve");
    assert!(map.contains_key(&tip));
}

#[test]
fn pack_objects_full_and_incremental_pass_git_index_pack_and_fsck() {
    let (dir, tip, have) = build_deep_history_repo(80);
    let odb = open_odb(dir.path());
    let full = grit_delta_pack(&odb, &[tip], &[]);
    git_index_pack_strict(dir.path(), &full);
    let inc = grit_delta_pack(&odb, &[tip], &[have]);
    git_index_pack_strict(dir.path(), &inc);
    let git_full = git_pack_objects(dir.path(), &[tip], &[]);
    assert!(
        full.len() <= git_full.len() * 11 / 10,
        "full pack size {} vs git {} (ratio {:.2})",
        full.len(),
        git_full.len(),
        full.len() as f64 / git_full.len() as f64
    );
}

#[test]
fn pack_objects_thin_pack_fixes_with_git_index_pack() {
    let (dir, tip, have) = build_deep_history_repo(40);
    let odb = open_odb(dir.path());
    let thin = {
        let mut buf = Vec::new();
        PackObjects::new(
            &odb,
            PackObjectsOptions::from(PackBuildOptions {
                delta: true,
                thin: true,
                ..PackBuildOptions::default()
            }),
        )
        .wants(&[tip])
        .haves(&[have])
        .write_to(&mut buf, &mut grit_lib::progress::NullProgress)
        .expect("thin");
        buf
    };
    git_index_pack_stdin_fix_thin(dir.path(), have, &thin);
    let map = pack_bytes_to_object_map(&thin, &odb).expect("resolve");
    assert!(map.contains_key(&tip));
}

#[test]
fn pack_objects_write_to_matches_build_pack_bytes() {
    let (dir, tip, have) = build_deep_history_repo(12);
    let odb = open_odb(dir.path());
    let opts = PackBuildOptions {
        delta: true,
        ..PackBuildOptions::default()
    };
    let via_build = build_pack(&odb, &[tip], &[have], &opts).expect("build_pack");
    let mut via_api = Vec::new();
    PackObjects::new(&odb, PackObjectsOptions::from(opts))
        .wants(&[tip])
        .haves(&[have])
        .write_to(&mut via_api, &mut grit_lib::progress::NullProgress)
        .expect("write_to");
    assert_eq!(
        via_build, via_api,
        "PackObjects must match build_pack output"
    );
}

// SHA-256 pack indexing is covered by `matrix_packs::sha256_repo_packs_index_and_fsck_clean`
// (same `build_pack` / `PackObjects` serializer and trailer width).

#[test]
fn reuse_only_write_to_streams_same_bytes_as_build_pack() {
    let (dir, tip, _have) = build_deep_history_repo(30);
    let odb = open_odb(dir.path());
    let opts = PackBuildOptions::for_repack_rewrite(None);
    let via_build = build_pack(&odb, &[tip], &[], &opts).expect("build_pack");
    let mut streamed = Vec::new();
    let stats = PackObjects::new(&odb, PackObjectsOptions::from(opts))
        .wants(&[tip])
        .write_to(&mut streamed, &mut grit_lib::progress::NullProgress)
        .expect("write_to");
    assert_eq!(
        streamed, via_build,
        "streamed reuse-only pack must match build_pack"
    );
    assert_eq!(stats.pack_bytes, streamed.len() as u64);
    assert!(stats.delta_count > 0, "on-disk deltas should be reused");
    git_index_pack_strict(dir.path(), &streamed);
}

#[test]
fn reuse_only_thin_pack_keeps_deltas_against_have_bases() {
    // Git stores the shorter, newer blob as a delta against the older one, so the
    // tip's blob has an on-disk base that only the have side holds.
    let dir = tempfile::tempdir().expect("tmpdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    let mut state = 7u64;
    let mut lines: Vec<String> = (0..3000)
        .map(|_| {
            (0..48)
                .map(|_| {
                    state = state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    char::from(b'a' + ((state >> 33) % 26) as u8)
                })
                .collect()
        })
        .collect();
    std::fs::write(dir.path().join("big.txt"), lines.join("\n")).unwrap();
    git(dir.path(), &["add", "big.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "big"]);
    let have = rev_parse(dir.path(), "HEAD");
    lines.remove(1500);
    std::fs::write(dir.path().join("big.txt"), lines.join("\n")).unwrap();
    git(dir.path(), &["commit", "-q", "-am", "drop a line"]);
    let tip = rev_parse(dir.path(), "HEAD");
    git(dir.path(), &["repack", "-adfq"]);
    let odb = open_odb(dir.path());
    let opts = PackBuildOptions {
        thin: true,
        ..PackBuildOptions::for_repack_rewrite(None)
    };
    let mut thin = Vec::new();
    let stats = PackObjects::new(&odb, PackObjectsOptions::from(opts))
        .wants(&[tip])
        .haves(&[have])
        .write_to(&mut thin, &mut grit_lib::progress::NullProgress)
        .expect("thin reuse-only pack");
    assert!(stats.delta_count > 0, "thin pack should reuse deltas");
    let appended = git_index_pack_stdin_fix_thin(dir.path(), have, &thin);
    assert!(
        appended > 0,
        "reused deltas should name bases only the receiver has"
    );
    let map = pack_bytes_to_object_map(&thin, &odb).expect("resolve");
    assert!(map.contains_key(&tip));
}

/// A wide repository: hundreds of files at the root and in `src/`, plus a larger file in
/// `doc/` that every commit edits. Returns the work tree.
fn build_wide_tree_history(commits: usize) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("doc")).unwrap();
    let body = |name: &str, version: usize| {
        (0..12)
            .map(|line| format!("{name} line {line} of a file that is long enough, v{version}\n"))
            .collect::<String>()
    };
    for i in 0..400 {
        let name = format!("root-file-{i:04}.txt");
        std::fs::write(dir.join(&name), body(&name, 0)).unwrap();
    }
    for i in 0..300 {
        let name = format!("module_{i:04}.rs");
        std::fs::write(dir.join("src").join(&name), body(&name, 0)).unwrap();
    }
    let mut guide: Vec<String> = (0..300)
        .map(|i| format!("guide paragraph {i} explains one more thing in some detail"))
        .collect();
    std::fs::write(dir.join("doc/guide.txt"), guide.join("\n")).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial"]);
    for c in 1..=commits {
        let at = (c * 37) % guide.len();
        guide[at] = format!("guide paragraph rewritten in commit {c}");
        std::fs::write(dir.join("doc/guide.txt"), guide.join("\n")).unwrap();
        let src = format!("module_{:04}.rs", (c * 7) % 300);
        std::fs::write(dir.join("src").join(&src), body(&src, c)).unwrap();
        let root = format!("root-file-{:04}.txt", (c * 13) % 400);
        std::fs::write(dir.join(&root), body(&root, c)).unwrap();
        git(dir, &["commit", "-q", "-am", &format!("change {c}")]);
    }
    tmp
}

/// Object kind counts of a pack: `(whole trees, whole blobs, REF_DELTA bases)`.
fn pack_entry_shape(pack: &[u8]) -> (usize, usize, HashSet<ObjectId>) {
    let count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as usize;
    let mut pos = 12usize;
    let (mut trees, mut blobs) = (0, 0);
    let mut ref_bases = HashSet::new();
    for _ in 0..count {
        let mut byte = pack[pos];
        pos += 1;
        let kind = (byte >> 4) & 7;
        while byte & 0x80 != 0 {
            byte = pack[pos];
            pos += 1;
        }
        match kind {
            2 => trees += 1,
            3 => blobs += 1,
            6 => {
                while pack[pos] & 0x80 != 0 {
                    pos += 1;
                }
                pos += 1;
            }
            7 => {
                ref_bases.insert(ObjectId::from_bytes(&pack[pos..pos + 20]).unwrap());
                pos += 20;
            }
            _ => {}
        }
        let mut inflate = flate2::Decompress::new(true);
        let mut sink = vec![0u8; 1 << 16];
        loop {
            let before = inflate.total_in();
            let status = inflate
                .decompress(&pack[pos..], &mut sink, flate2::FlushDecompress::None)
                .expect("inflate pack entry");
            pos += (inflate.total_in() - before) as usize;
            if status == flate2::Status::StreamEnd {
                break;
            }
        }
    }
    (trees, blobs, ref_bases)
}

/// The pack upload-pack sends a thin-pack client that has `have` and wants `want`.
fn upload_pack_thin(repo_dir: &Path, want: ObjectId, have: ObjectId) -> Vec<u8> {
    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::objects::pack_objects::write_upload_pack_stream;
    use grit_lib::repo::Repository;
    let repo = Repository::open(&repo_dir.join(".git"), &RepositoryOptions::empty()).unwrap();
    let mut pack = Vec::new();
    write_upload_pack_stream(
        &repo,
        &[want],
        &[have],
        &HashSet::new(),
        &HashSet::new(),
        None,
        &PackBuildOptions::for_serve(None, true, true),
        &mut |_| {},
        &mut pack,
    )
    .expect("upload-pack stream");
    pack
}

fn assert_thin_upload_pack_matches_git(back: usize, bitmap: bool) {
    let tmp = build_wide_tree_history(24);
    let dir = tmp.path();
    if bitmap {
        git(dir, &["repack", "-adbq"]);
        git(dir, &["commit-graph", "write", "--reachable"]);
    } else {
        git(dir, &["repack", "-adq"]);
    }
    assert_eq!(
        dir.join(".git/objects/pack")
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|x| x == "bitmap")),
        bitmap
    );
    let want = rev_parse(dir, "HEAD");
    let have = rev_parse(dir, &format!("HEAD~{back}"));
    let pack = upload_pack_thin(dir, want, have);

    git_index_pack_stdin_fix_thin(dir, have, &pack);
    let (whole_trees, whole_blobs, ref_bases) = pack_entry_shape(&pack);
    let old_root = rev_parse(dir, &format!("{have}^{{tree}}"));
    let old_src = rev_parse(dir, &format!("{have}:src"));
    let old_guide = rev_parse(dir, &format!("{have}:doc/guide.txt"));
    for (what, oid) in [
        ("root tree", old_root),
        ("src tree", old_src),
        ("guide blob", old_guide),
    ] {
        assert!(
            ref_bases.contains(&oid),
            "the client's {what} is a REF_DELTA base (back {back}, bitmap {bitmap})"
        );
    }
    let git_thin = git_pack_objects_with_thin(dir, &[want], &[have], true);
    if back == 1 {
        let (git_trees, git_blobs, _) = pack_entry_shape(&git_thin);
        assert!(
            whole_trees <= git_trees && whole_blobs <= git_blobs,
            "a one-commit increment sends no more whole trees ({whole_trees} vs {git_trees}) \
             or blobs ({whole_blobs} vs {git_blobs}) than git (bitmap {bitmap})"
        );
    }
    assert!(
        pack.len() <= git_thin.len() * 5 / 4 + 256,
        "grit thin pack {} bytes vs git pack-objects --thin {} (back {back}, bitmap {bitmap})",
        pack.len(),
        git_thin.len()
    );
}

/// Issue #2135: changed trees and the previous version of a changed blob are sent as
/// REF_DELTAs against what the client holds, not whole, with and without a bitmap.
#[test]
fn thin_upload_pack_one_commit_deltas_trees_and_blobs_against_client_objects() {
    assert_thin_upload_pack_matches_git(1, false);
    assert_thin_upload_pack_matches_git(1, true);
}

#[test]
fn thin_upload_pack_twenty_commits_stays_close_to_git_size() {
    assert_thin_upload_pack_matches_git(20, false);
    assert_thin_upload_pack_matches_git(20, true);
}

/// A bitmap must not make a thin incremental pack bigger than the same repository
/// without one.
#[test]
fn thin_upload_pack_with_bitmap_is_no_bigger_than_without() {
    let tmp = build_wide_tree_history(24);
    let dir = tmp.path();
    git(dir, &["repack", "-adbq"]);
    let want = rev_parse(dir, "HEAD");
    let have = rev_parse(dir, "HEAD~20");
    let with_bitmap = upload_pack_thin(dir, want, have);
    for entry in dir.join(".git/objects/pack").read_dir().unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|x| x == "bitmap") {
            std::fs::remove_file(path).unwrap();
        }
    }
    let without = upload_pack_thin(dir, want, have);
    git_index_pack_stdin_fix_thin(dir, have, &with_bitmap);
    git_index_pack_stdin_fix_thin(dir, have, &without);
    assert!(
        with_bitmap.len() <= without.len() * 11 / 10 + 256,
        "bitmap pack {} bytes vs {} without the bitmap",
        with_bitmap.len(),
        without.len()
    );
}

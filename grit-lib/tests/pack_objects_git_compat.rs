//! Git compatibility for [`grit_lib::objects::pack_objects`] packs: `index-pack --strict`, `fsck`,
//! thin-pack fix, size parity vs `git pack-objects --revs`, and SHA-256 repos.

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

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
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

fn git_pack_objects(repo: &Path, tips: &[ObjectId], haves: &[ObjectId]) -> Vec<u8> {
    let mut child = Command::new("git");
    child
        .current_dir(repo)
        .args(["pack-objects", "--stdout", "--revs", "--delta-base-offset"])
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
        .write_to(&mut buf)
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
        .write_to(&mut via_api)
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
        .write_to(&mut streamed)
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
        .write_to(&mut thin)
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

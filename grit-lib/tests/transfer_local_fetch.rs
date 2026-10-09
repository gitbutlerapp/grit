//! Integration tests for `grit_lib::transfer::fetch_local` — the in-process
//! local / `file://` fetch path (no subprocess, no wire protocol).
//!
//! A remote repo is built with the system `git`; an empty local repo is
//! initialized; `fetch_local` copies refs + the minimal object set into the
//! local odb. We assert the tracking refs, the presence of the transferred
//! objects, the per-ref `UpdateMode`, and the resolved default branch — then
//! repeat after advancing the remote to assert a `FastForward`.

use std::path::Path;
use std::process::Command;

use grit_lib::config::ConfigSet;
use grit_lib::environment::Environment;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{
    build_pack, fetch_local, FetchOptions, PackBuildOptions, TagMode, UpdateMode,
};
use grit_test_support::{git, git_cmd};

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn open_odb(git_dir: &Path) -> Odb {
    Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf())
}

/// Build a remote with two commits on `main` plus an annotated tag.
fn build_remote(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "c2"]);
    git(dir, &["tag", "-a", "v1", "-m", "release one"]);
}

#[test]
fn fetch_local_copies_refs_and_objects_then_fast_forwards() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    build_remote(&remote);
    // Bare-style git_dir for both is just the `.git` directory.
    let remote_git = remote.join(".git");

    // Empty local repo.
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let remote_main = rev_parse(&remote, "refs/heads/main");
    let remote_c1 = rev_parse(&remote, "HEAD~1");
    let remote_v1 = rev_parse(&remote, "refs/tags/v1");

    // --- First fetch: everything is New. ---
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::All,
        ..Default::default()
    };
    let outcome = fetch_local(&local_git, &remote_git, &opts).expect("fetch_local");

    // default_branch from remote HEAD symref.
    assert_eq!(outcome.default_branch.as_deref(), Some("main"));

    // Tracking ref written and equals the remote tip.
    let tracked = resolve_ref(&local_git, "refs/remotes/origin/main").expect("tracking ref");
    assert_eq!(tracked, remote_main);

    // Objects present locally: read them back from the local odb.
    let local_odb = open_odb(&local_git);
    let c2 = local_odb.read(&remote_main).expect("commit c2 present");
    assert_eq!(c2.kind, ObjectKind::Commit);
    assert!(local_odb.exists(&remote_c1), "parent commit c1 present");
    // The b.txt blob introduced by c2 must be present (proves tree+blob copied).
    let b_blob = rev_parse(&remote, "HEAD:b.txt");
    let blob = local_odb.read(&b_blob).expect("b.txt blob present");
    assert_eq!(blob.kind, ObjectKind::Blob);
    assert_eq!(blob.data, b"two\n");

    // The annotated tag object and its tracking ref are present.
    assert!(local_odb.exists(&remote_v1), "tag object present");
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/v1").expect("tag ref"),
        remote_v1
    );

    // Every head/tag update was New.
    let head_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("main update present");
    assert_eq!(head_update.mode, UpdateMode::New);
    assert_eq!(head_update.new_oid, Some(remote_main));
    assert!(head_update.old_oid.is_none());
    for u in &outcome.updates {
        if u.mode == UpdateMode::DeletedMissing {
            continue;
        }
        assert_eq!(u.mode, UpdateMode::New, "ref {} not New", u.remote_ref);
    }

    // --- Advance the remote, fetch again: FastForward. ---
    std::fs::write(remote.join("c.txt"), "three\n").unwrap();
    git(&remote, &["add", "c.txt"]);
    git(&remote, &["commit", "-q", "-m", "c3"]);
    let remote_main2 = rev_parse(&remote, "refs/heads/main");
    let c_blob = rev_parse(&remote, "HEAD:c.txt");
    assert_ne!(remote_main2, remote_main);

    let outcome2 = fetch_local(&local_git, &remote_git, &opts).expect("second fetch");

    let head_update2 = outcome2
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .expect("main update present (2)");
    assert_eq!(head_update2.mode, UpdateMode::FastForward);
    assert_eq!(head_update2.old_oid, Some(remote_main));
    assert_eq!(head_update2.new_oid, Some(remote_main2));

    // Tracking ref advanced and the new objects arrived.
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").unwrap(),
        remote_main2
    );
    let local_odb = open_odb(&local_git);
    assert!(local_odb.exists(&remote_main2), "new commit present");
    let c_blob_obj = local_odb.read(&c_blob).expect("c.txt blob present");
    assert_eq!(c_blob_obj.data, b"three\n");

    // The tag did not move, so it is UpToDate on the second fetch.
    let tag_update2 = outcome2
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/tags/v1");
    if let Some(u) = tag_update2 {
        assert_eq!(u.mode, UpdateMode::UpToDate);
    }
}

#[test]
fn pack_build_honors_explicit_zero_window_and_depth() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    for i in 0..12 {
        let mut body = String::from("shared prefix\n");
        body.push_str(&"z".repeat(4096));
        body.push_str(&format!("tail-{i}\n"));
        std::fs::write(remote.join(format!("b{i}.txt")), body).unwrap();
    }
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-qm", "similar blobs"]);
    git(
        &remote,
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
    git(&remote, &["config", "pack.window", "0"]);
    git(&remote, &["config", "pack.depth", "0"]);

    let remote_git = remote.join(".git");
    let cfg = ConfigSet::load(&Environment::empty(), Some(&remote_git), true).expect("config");
    assert_eq!(cfg.pack_object_window(), 0);
    assert_eq!(cfg.pack_object_depth(), 0);

    let odb = open_odb(&remote_git);
    let tip = rev_parse(&remote, "HEAD");
    let pack = build_pack(
        &odb,
        &[tip],
        &[],
        &PackBuildOptions::for_local_copy(Some(&cfg)),
    )
    .expect("build");

    let scratch = tempfile::tempdir().expect("scratch");
    let pack_path = scratch.path().join("z.pack");
    std::fs::write(&pack_path, &pack).unwrap();
    let idx_out = Command::new("git")
        .current_dir(scratch.path())
        .args(["index-pack", "z.pack"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        idx_out.status.success(),
        "index-pack failed: {}",
        String::from_utf8_lossy(&idx_out.stderr)
    );
    let idx_path = std::fs::read_dir(scratch.path())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("idx beside pack");
    let out = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&idx_path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("verify-pack");
    assert!(
        out.status.success(),
        "verify-pack failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let new_deltas = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.split_whitespace().count() >= 7)
        .count();
    assert_eq!(
        new_deltas, 0,
        "window=0 depth=0 must not emit newly computed deltas"
    );
}

fn pack_files(git_dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(git_dir.join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect()
}

#[test]
fn fetch_local_from_delta_packed_remote_produces_compact_pack() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();

    git(&remote, &["init", "-q", "-b", "main", "."]);
    for i in 0..24 {
        let mut body = String::from("shared blob prefix for delta compression\n");
        body.push_str(&"q".repeat(48_000));
        body.push_str(&format!("\nunique suffix {i}\n"));
        std::fs::write(remote.join(format!("blob{i}.txt")), body).unwrap();
    }
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-q", "-m", "many similar blobs"]);

    let remote_git = remote.join(".git");
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        ..Default::default()
    };
    fetch_local(&local_git, &remote_git, &opts).expect("fetch_local");

    let packs = pack_files(&local_git);
    assert_eq!(packs.len(), 1, "expected one installed pack");

    let grit_pack_len = std::fs::metadata(&packs[0]).expect("pack metadata").len();
    let remote_odb = open_odb(&remote_git);
    let tip = rev_parse(&remote, "HEAD");
    let undeltified = build_pack(&remote_odb, &[tip], &[], &PackBuildOptions::default())
        .expect("undeltified pack for comparison");
    assert!(
        grit_pack_len < undeltified.len() as u64,
        "fetch_local must delta-compress blobs (grit={grit_pack_len}, undeltified={})",
        undeltified.len()
    );

    let count_out = git(&local_git, &["count-objects", "-v"]);
    let mut loose = u64::MAX;
    for line in count_out.lines() {
        if let Some(v) = line.strip_prefix("count: ") {
            loose = v.trim().parse().expect("count");
        }
    }
    assert_eq!(
        loose, 0,
        "fetch_local should install a pack, not loose objects"
    );

    let fsck = git_cmd(&["fsck", "--strict"]).in_dir(&local_git).exec();
    assert!(fsck.ok(), "fsck after fetch: {}", fsck.stderr);
}

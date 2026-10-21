//! Local (`file://`) clone and push from a source whose objects are split
//! between a packfile and loose files.
//!
//! The local transfer links the source's packs into the destination, then packs
//! only the objects that are still missing (the loose ones). These tests check
//! that the second pack holds exactly those objects instead of re-copying the
//! whole history, and that the system `git` accepts the result.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::Environment;
use grit_lib::objects::ObjectId;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{push_local, PushOptions, PushRefSpec};
use grit_test_support::{git, git_cmd};

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) {
    if let Some(parent) = Path::new(name).parent() {
        std::fs::create_dir_all(dir.join(parent)).unwrap();
    }
    std::fs::write(dir.join(name), body).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// History in one pack, then annotated tags and a new commit left loose.
///
/// Returns the number of reachable objects that exist only as loose files.
fn build_mixed_source(dir: &Path) -> usize {
    git(dir, &["init", "-q", "-b", "main", "."]);
    for i in 0..6 {
        let body = format!("line {i}\n").repeat(40);
        commit_file(dir, &format!("src/file{i}.txt"), &body, &format!("c{i}"));
        commit_file(dir, "shared.txt", &body, &format!("shared {i}"));
    }
    git(dir, &["repack", "-a", "-d", "-q"]);
    assert_eq!(loose_objects(&dir.join(".git/objects")).len(), 0);

    for i in 0..4 {
        git(
            dir,
            &[
                "tag",
                "-a",
                &format!("v{i}"),
                "-m",
                &format!("release {i}"),
                &format!("HEAD~{i}"),
            ],
        );
    }
    git(dir, &["switch", "-q", "-c", "topic"]);
    commit_file(dir, "src/topic.txt", "topic\n", "topic");
    git(dir, &["switch", "-q", "main"]);

    // 4 tags + topic commit + its root tree + `src` tree + blob.
    let loose = loose_objects(&dir.join(".git/objects"));
    assert_eq!(loose.len(), 8, "fixture loose objects: {loose:?}");
    loose.len()
}

fn loose_objects(objects: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for shard in std::fs::read_dir(objects).unwrap().flatten() {
        let name = shard.file_name().to_string_lossy().into_owned();
        if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        for obj in std::fs::read_dir(shard.path()).unwrap().flatten() {
            out.insert(format!("{name}{}", obj.file_name().to_string_lossy()));
        }
    }
    out
}

/// Object count of every pack under `objects/pack`, keyed by pack file name.
fn pack_object_counts(objects: &Path) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(objects.join("pack")).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("idx") {
            continue;
        }
        let idx = std::fs::read(&path).unwrap();
        // v2 index: magic, version, 256-entry fanout; the last entry is the count.
        let count = u32::from_be_bytes(idx[8 + 255 * 4..8 + 256 * 4].try_into().unwrap());
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        out.push((name, count as usize));
    }
    out.sort();
    out
}

fn all_objects(git_dir: &Path) -> BTreeSet<String> {
    git_cmd(&["rev-list", "--objects", "--all"])
        .in_dir(git_dir)
        .suc()
        .stdout
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect()
}

fn fsck_strict(git_dir: &Path) {
    let out = git_cmd(&["fsck", "--strict", "--no-dangling"])
        .in_dir(git_dir)
        .exec();
    assert!(out.ok(), "git fsck failed: {}{}", out.stdout, out.stderr);
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.canonicalize().unwrap().display())
}

fn grit_clone(url: String, dest: PathBuf) {
    clone_repo(
        CloneOptions {
            url,
            dest,
            environment: Environment::capture_process(),
            remote_name: grit_lib::remote::DEFAULT_REMOTE.to_owned(),
            initial_branch: "main".to_owned(),
            thread_policy: grit_lib::parallel::ThreadPolicy::default(),
        },
        &mut NoProgress,
        None,
    )
    .expect("grit clone");
}

#[test]
fn file_clone_packs_only_loose_objects_beside_linked_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let loose_count = build_mixed_source(&src);
    let src_packs = pack_object_counts(&src.join(".git/objects"));
    assert_eq!(src_packs.len(), 1);

    let dest = tmp.path().join("dest");
    grit_clone(file_url(&src), dest.clone());

    let dest_git = dest.join(".git");
    let mut packs = pack_object_counts(&dest_git.join("objects"));
    let linked = packs.iter().position(|p| *p == src_packs[0]);
    let linked = linked.expect("source pack linked into the clone");
    packs.remove(linked);
    let copied: usize = packs.iter().map(|(_, n)| n).sum();
    assert_eq!(
        copied, loose_count,
        "only the loose objects should be packed again: {packs:?}"
    );

    fsck_strict(&dest_git);
    assert_eq!(all_objects(&dest_git), all_objects(&src.join(".git")));
    assert_eq!(
        rev_parse(&dest, "refs/remotes/origin/topic"),
        rev_parse(&src, "refs/heads/topic")
    );
    assert_eq!(rev_parse(&dest, "v3^{commit}"), rev_parse(&src, "main~3"));
}

#[test]
fn file_clone_of_loose_only_source_is_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main", "."]);
    for i in 0..8 {
        let body = format!("revision {i}\n{}", "stable text\n".repeat(200));
        commit_file(&src, "dir/a.txt", &body, &format!("a{i}"));
        commit_file(&src, "dir/sub/b.txt", &body, &format!("b{i}"));
    }
    git(&src, &["tag", "-a", "v1", "-m", "one"]);
    assert!(!loose_objects(&src.join(".git/objects")).is_empty());

    let dest = tmp.path().join("dest");
    grit_clone(file_url(&src), dest.clone());

    let dest_git = dest.join(".git");
    fsck_strict(&dest_git);
    assert_eq!(all_objects(&dest_git), all_objects(&src.join(".git")));
    assert_eq!(rev_parse(&dest, "HEAD"), rev_parse(&src, "HEAD"));
}

#[test]
fn push_to_empty_bare_packs_only_loose_objects_beside_linked_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let remote = tmp.path().join("remote.git");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    let loose_count = build_mixed_source(&src);
    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    let src_packs = pack_object_counts(&src.join(".git/objects"));

    let refs = [
        "refs/heads/main",
        "refs/heads/topic",
        "refs/tags/v0",
        "refs/tags/v1",
        "refs/tags/v2",
        "refs/tags/v3",
    ];
    let specs: Vec<PushRefSpec> = refs
        .into_iter()
        .map(|name| PushRefSpec {
            src: Some(rev_parse(&src, name)),
            dst: name.to_owned(),
            force: false,
            delete: false,
            expected_old: None,
            expect_absent: false,
        })
        .collect();
    push_local(&src.join(".git"), &remote, &specs, &PushOptions::default()).expect("push");

    let mut packs = pack_object_counts(&remote.join("objects"));
    let linked = packs.iter().position(|p| *p == src_packs[0]);
    let linked = linked.expect("source pack linked into the remote");
    packs.remove(linked);
    let copied: usize = packs.iter().map(|(_, n)| n).sum();
    assert_eq!(
        copied, loose_count,
        "only the loose objects should be packed again: {packs:?}"
    );

    fsck_strict(&remote);
    assert_eq!(all_objects(&remote), all_objects(&src.join(".git")));
}

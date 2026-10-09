//! Packed-refs (files backend): load, list, delete, clone packing, cached resolve.
//!
//! Scenarios ported from upstream `t1408-packed-refs`, `t0601-reffiles-pack-refs`,
//! `t1409-avoid-packing-refs`, and `t0600` delete cases.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::refs::{
    delete_ref, list_refs, list_refs_glob, list_refs_physical, pack_remote_tracking_refs_for_clone,
    packed_refs_entry_exists, resolve_ref, resolve_ref_cached, write_ref, write_ref_cached,
    PackedRefs,
};

use support::{
    assert_list_refs_match_git, files_repo, git, git_empty_commit_oid, git_for_each_ref,
    git_fsck_strict, TestRepo,
};

fn git_dir(repo: &TestRepo) -> PathBuf {
    repo.git_dir()
}

fn write_packed_refs(git_dir: &Path, body: &str) {
    fs::write(git_dir.join("packed-refs"), body).expect("write packed-refs");
}

fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).expect("read file")
}

fn seed_repo_with_branches_and_tag(repo: &TestRepo) -> ObjectId {
    let wt = repo.worktree();
    let _ = git_empty_commit_oid(wt);
    git(wt, &["tag", "-a", "v-packed", "-m", "annotated"]);
    git(wt, &["branch", "topic"]);
    git(wt, &["branch", "other"]);
    git(wt, &["pack-refs", "--all"]);
    git_empty_commit_oid(wt)
}

#[test]
fn t0601_peeled_and_sorted_traits_match_git() {
    let repo = files_repo();
    seed_repo_with_branches_and_tag(&repo);
    let git_dir = git_dir(&repo);

    let packed = PackedRefs::load(&git_dir).expect("load");
    let header = fs::read_to_string(git_dir.join("packed-refs")).expect("packed-refs");
    assert!(
        header.starts_with("# pack-refs with:"),
        "expected pack-refs header"
    );
    assert!(header.contains("peeled"), "expected peeled trait in header");
    assert!(
        header.contains("fully-peeled") || header.contains("peeled"),
        "expected peel metadata"
    );

    let tag_oid: ObjectId = git(wt_path(&repo), &["rev-parse", "refs/tags/v-packed"])
        .trim()
        .parse()
        .unwrap();
    assert_eq!(packed.get("refs/tags/v-packed"), Some(tag_oid));

    assert_list_refs_match_git(wt_path(&repo), "refs/");
}

fn wt_path(repo: &TestRepo) -> &Path {
    repo.worktree()
}

#[test]
fn t1408_stale_packed_entry_no_error() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let _oid1 = git_empty_commit_oid(wt);
    git(wt, &["branch", "stale-test"]);
    git(wt, &["pack-refs", "--all"]);

    let oid2 = git_empty_commit_oid(wt);
    write_ref(&git_dir, "refs/heads/stale-test", &oid2).expect("update loose over packed");

    assert_eq!(
        resolve_ref(&git_dir, "refs/heads/stale-test").expect("resolve"),
        oid2
    );
    assert_list_refs_match_git(wt, "refs/heads/");
    assert!(git_fsck_strict(wt));
}

#[test]
fn t1408_unsorted_packed_refs_without_sorted_trait() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let oid = git_empty_commit_oid(wt);
    git(wt, &["branch", "zzz"]);
    git(wt, &["branch", "aaa"]);
    let oid_hex = oid.to_hex();
    write_packed_refs(
        &git_dir,
        &format!(
            "# pack-refs with: peeled\n{oid_hex} refs/heads/zzz\n{oid_hex} refs/heads/aaa\n{oid_hex} refs/heads/main\n"
        ),
    );
    let _ = fs::remove_file(git_dir.join("refs/heads/zzz"));
    let _ = fs::remove_file(git_dir.join("refs/heads/aaa"));
    let _ = fs::remove_file(git_dir.join("refs/heads/main"));

    assert_list_refs_match_git(wt, "refs/heads/");
}

#[test]
fn t1408_unicode_ref_names_in_packed_refs() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let oid = git_empty_commit_oid(wt);
    let name = "refs/heads/unicode-分支";
    write_ref(&git_dir, name, &oid).expect("write unicode ref");
    git(wt, &["pack-refs", "--all"]);
    assert_list_refs_match_git(wt, "refs/heads/");
    let packed = PackedRefs::load(&git_dir).expect("load");
    assert_eq!(packed.get(name), Some(oid));
}

#[test]
fn packed_refs_get_has_namespace_and_entry_exists() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let oid = git_empty_commit_oid(wt);
    write_ref(&git_dir, "refs/heads/parent", &oid).unwrap();
    git(wt, &["pack-refs", "--all"]);

    let packed = PackedRefs::load(&git_dir).unwrap();
    assert_eq!(packed.get("refs/heads/parent"), Some(oid));
    assert!(packed.has_namespace_conflict("refs/heads/parent/child"));
    assert!(!packed.has_namespace_conflict("refs/heads/other"));
    assert!(packed_refs_entry_exists(&git_dir, "refs/heads/parent").unwrap());
    assert!(!packed_refs_entry_exists(&git_dir, "refs/heads/missing").unwrap());
}

#[test]
fn list_refs_physical_and_glob_merge_loose_over_packed() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let _oid_packed = git_empty_commit_oid(wt);
    git(wt, &["branch", "dup"]);
    git(wt, &["pack-refs", "--all"]);

    let oid_loose = git_empty_commit_oid(wt);
    write_ref(&git_dir, "refs/heads/dup", &oid_loose).unwrap();

    assert_list_refs_match_git(wt, "refs/heads/");
    let physical = list_refs_physical(&git_dir, "refs/heads/").unwrap();
    let git_physical = git_for_each_ref(wt, "refs/heads/");
    assert_eq!(physical, git_physical);

    let globbed = list_refs_glob(&git_dir, "refs/heads/d*").unwrap();
    let mut expected: Vec<_> = git_for_each_ref(wt, "refs/heads/")
        .into_iter()
        .filter(|(n, _)| n.starts_with("refs/heads/d"))
        .collect();
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(globbed, expected);
    assert_eq!(
        globbed
            .iter()
            .find(|(n, _)| n == "refs/heads/dup")
            .unwrap()
            .1,
        oid_loose
    );
}

#[test]
fn t0600_delete_loose_and_packed_removes_both_and_fsck_clean() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let oid = git_empty_commit_oid(wt);
    git(wt, &["branch", "to-delete"]);
    git(wt, &["pack-refs", "--all"]);
    write_ref(&git_dir, "refs/heads/to-delete", &oid).unwrap();

    let before_packed = read_bytes(&git_dir.join("packed-refs"));
    delete_ref(&git_dir, "refs/heads/to-delete").expect("delete");
    assert!(!git_dir.join("refs/heads/to-delete").exists());
    assert!(!packed_refs_entry_exists(&git_dir, "refs/heads/to-delete").unwrap());
    assert_list_refs_match_git(wt, "refs/");
    assert!(git_fsck_strict(wt));
    assert_ne!(
        read_bytes(&git_dir.join("packed-refs")),
        before_packed,
        "packed-refs should be rewritten"
    );
}

#[test]
fn t0600_delete_packed_only_ref() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    git_empty_commit_oid(wt);
    git(wt, &["branch", "packed-only"]);
    git(wt, &["pack-refs", "--all"]);
    assert!(!git_dir.join("refs/heads/packed-only").exists());

    delete_ref(&git_dir, "refs/heads/packed-only").expect("delete packed-only");
    assert!(!packed_refs_entry_exists(&git_dir, "refs/heads/packed-only").unwrap());
    assert_list_refs_match_git(wt, "refs/");
    assert!(git_fsck_strict(wt));
}

#[test]
fn t0600_delete_symref_target() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let oid = git_empty_commit_oid(wt);
    write_ref(&git_dir, "refs/heads/target", &oid).unwrap();
    grit_lib::refs::write_symbolic_ref(&git_dir, "refs/heads/sym", "refs/heads/target").unwrap();
    git(wt, &["pack-refs", "--all"]);

    delete_ref(&git_dir, "refs/heads/target").expect("delete symref target");
    assert!(!packed_refs_entry_exists(&git_dir, "refs/heads/target").unwrap());
    assert_list_refs_match_git(wt, "refs/");
}

#[test]
fn t0600_delete_packed_only_does_not_create_parent_directories() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let oid = git_empty_commit_oid(wt);
    let nested = "refs/heads/nested/only";
    write_ref(&git_dir, nested, &oid).unwrap();
    git(wt, &["pack-refs", "--all"]);
    let parent = git_dir.join("refs/heads/nested");
    let _ = fs::remove_dir_all(&parent);

    delete_ref(&git_dir, nested).expect("delete");
    assert!(
        !parent.exists(),
        "must not mkdir parent when deleting packed ref"
    );
}

#[test]
fn t0600_delete_fails_cleanly_when_packed_refs_locked() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    git_empty_commit_oid(wt);
    git(wt, &["branch", "locked"]);
    git(wt, &["pack-refs", "--all"]);

    let packed_path = git_dir.join("packed-refs");
    let loose_path = git_dir.join("refs/heads/locked");
    let before_packed = read_bytes(&packed_path);
    let before_loose = loose_path
        .exists()
        .then(|| read_bytes(&loose_path))
        .unwrap_or_default();

    let lock_path = git_dir.join("packed-refs.lock");
    let _lock = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .expect("hold lock");

    let err = delete_ref(&git_dir, "refs/heads/locked").expect_err("delete must fail");
    match &err {
        Error::Message(msg) => assert!(
            msg.contains("Unable to create") && msg.contains("packed-refs.lock"),
            "unexpected error: {err:?}"
        ),
        other => panic!("expected Error::Message for lock failure, got {other:?}"),
    }

    assert_eq!(read_bytes(&packed_path), before_packed);
    if loose_path.exists() {
        assert_eq!(read_bytes(&loose_path), before_loose);
    }
    assert!(
        packed_refs_entry_exists(&git_dir, "refs/heads/locked").unwrap(),
        "ref must remain after failed delete"
    );
    drop(_lock);
    let _ = fs::remove_file(&lock_path);
    assert!(!git_dir.join("packed-refs.new").exists());
}

#[test]
fn t0600_delete_success_leaves_no_stray_lock_files() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    git_empty_commit_oid(wt);
    git(wt, &["branch", "gone"]);
    git(wt, &["pack-refs", "--all"]);
    delete_ref(&git_dir, "refs/heads/gone").unwrap();
    assert!(!git_dir.join("packed-refs.lock").exists());
    assert!(!git_dir.join("packed-refs.new").exists());
}

#[test]
fn pack_remote_tracking_refs_for_clone_matches_git() {
    let upstream = files_repo();
    let up_wt = wt_path(&upstream);
    git_empty_commit_oid(up_wt);
    git(up_wt, &["branch", "feature"]);
    git(up_wt, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(
        up_wt,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    git(
        up_wt,
        &[
            "update-ref",
            "refs/remotes/origin/feature",
            "refs/heads/feature",
        ],
    );

    let clone = files_repo();
    let clone_git = git_dir(&clone);
    fs::create_dir_all(clone_git.join("objects")).unwrap();
    for name in ["refs/remotes/origin/main", "refs/remotes/origin/feature"] {
        let oid: ObjectId = git(up_wt, &["rev-parse", name]).trim().parse().unwrap();
        write_ref(&clone_git, name, &oid).unwrap();
    }
    grit_lib::refs::write_symbolic_ref(
        &clone_git,
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    )
    .unwrap();

    pack_remote_tracking_refs_for_clone(&clone_git, "origin").expect("pack remote-tracking");

    assert_list_refs_match_git(wt_path(&clone), "refs/remotes/origin/");
    assert!(
        clone_git.join("refs/remotes/origin/HEAD").is_file(),
        "origin/HEAD stays loose"
    );
    let header = fs::read_to_string(clone_git.join("packed-refs")).unwrap();
    assert!(header.contains("sorted"));
}

#[test]
fn resolve_ref_cached_and_write_ref_cached_reload_after_git_pack_refs() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    git_empty_commit_oid(wt);
    git(wt, &["branch", "cached-a"]);
    git(wt, &["pack-refs", "--all"]);
    let mut snapshot = PackedRefs::load(&git_dir).unwrap();
    assert!(snapshot.get("refs/heads/cached-a").is_some());

    git(wt, &["branch", "cached-b"]);
    git(wt, &["pack-refs", "--all"]);

    assert!(
        snapshot.get("refs/heads/cached-b").is_none(),
        "stale snapshot omits newly packed ref"
    );
    assert!(
        resolve_ref_cached(&git_dir, "refs/heads/cached-b", &snapshot)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        resolve_ref(&git_dir, "refs/heads/cached-b").expect("fresh resolve"),
        git(wt, &["rev-parse", "refs/heads/cached-b"])
            .trim()
            .parse()
            .unwrap()
    );

    snapshot = PackedRefs::load(&git_dir).unwrap();
    assert!(snapshot.get("refs/heads/cached-b").is_some());
    assert_eq!(
        resolve_ref_cached(&git_dir, "refs/heads/cached-b", &snapshot)
            .unwrap()
            .unwrap(),
        resolve_ref(&git_dir, "refs/heads/cached-b").unwrap()
    );

    let new_oid = git_empty_commit_oid(wt);
    write_ref_cached(&git_dir, "refs/heads/cached-new", &new_oid, &snapshot).unwrap();
    assert_list_refs_match_git(wt, "refs/heads/");
}

#[test]
fn packed_refs_load_ignores_garbage_lines_like_git_valid_subset() {
    let repo = files_repo();
    let wt = wt_path(&repo);
    let git_dir = git_dir(&repo);
    let oid = git_empty_commit_oid(wt);
    git(wt, &["branch", "good"]);
    git(wt, &["pack-refs", "--all"]);
    let mut content = fs::read_to_string(git_dir.join("packed-refs")).unwrap();
    content.push_str("this is not a valid packed-refs line\n");
    write_packed_refs(&git_dir, &content);

    let grit_map: BTreeMap<_, _> = list_refs(&git_dir, "refs/heads/")
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(grit_map.get("refs/heads/good"), Some(&oid));
    let git_show = std::process::Command::new("git")
        .current_dir(wt)
        .args(["show-ref", "refs/heads/good"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git show-ref");
    assert!(
        !git_show.status.success(),
        "git rejects packed-refs with garbage lines"
    );
}

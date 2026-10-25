//! Repack, gc, prune and fsck in shallow clones, checked against the system `git`.
//!
//! A shallow clone records its boundary commits in `.git/shallow`; their parents are
//! absent and every history walk must treat them as parentless, as Git does.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::{self as fsck, FsckIssue, FsckOptions};
use grit_lib::maintenance::{
    gc, prune_loose_objects, repack_all, GcOptions, PruneOptions, RepackOptions,
};
use grit_lib::objects::pack::{clear_pack_cache, test_pack_cache_guard};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::gc::prune_loose_unreachable;
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git_output(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_output(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_oid(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("oid")
}

/// A source repository with `commits` commits, each adding a file and editing a shared one.
fn source_repo(root: &Path, commits: usize) -> std::path::PathBuf {
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("mkdir src");
    git(&src, &["init", "-q", "-b", "main"]);
    for i in 0..commits {
        std::fs::write(src.join(format!("f{i}.txt")), format!("file {i}\n")).expect("write");
        std::fs::write(src.join("shared.txt"), format!("rev {i}\n")).expect("write");
        git(&src, &["add", "."]);
        git(&src, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    src
}

/// `git clone --depth <depth>` over `file://` (local paths ignore `--depth`).
fn shallow_clone(
    root: &Path,
    src: &Path,
    name: &str,
    depth: usize,
    bare: bool,
) -> std::path::PathBuf {
    let dst = root.join(name);
    let url = format!("file://{}", src.display());
    let depth = depth.to_string();
    let mut args = vec!["clone", "-q", "--depth", depth.as_str()];
    if bare {
        args.push("--bare");
    }
    args.push(url.as_str());
    args.push(dst.to_str().expect("utf8 path"));
    git(root, &args);
    dst
}

fn git_dir_of(work: &Path) -> std::path::PathBuf {
    let dot_git = work.join(".git");
    if dot_git.is_dir() {
        dot_git
    } else {
        work.to_path_buf()
    }
}

/// Replace the clone's packs with loose objects (`git unpack-objects`).
fn explode_packs(work: &Path) {
    let pack_dir = git_dir_of(work).join("objects/pack");
    let packs: Vec<_> = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect();
    for pack in packs {
        let bytes = std::fs::read(&pack).expect("read pack");
        for ext in ["pack", "idx", "rev", "bitmap"] {
            let _ = std::fs::remove_file(pack.with_extension(ext));
        }
        let mut child = Command::new("git")
            .current_dir(work)
            .args(["unpack-objects", "-q"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn unpack-objects");
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(&bytes)
            .expect("feed pack");
        assert!(child.wait().expect("wait").success(), "unpack-objects");
    }
    clear_pack_cache();
}

fn shallow_boundaries(work: &Path) -> Vec<ObjectId> {
    std::fs::read_to_string(git_dir_of(work).join("shallow"))
        .expect("shallow file")
        .lines()
        .map(|l| ObjectId::from_hex(l.trim()).expect("shallow oid"))
        .collect()
}

/// Object ids `git rev-list --all --objects` reports (honouring `.git/shallow`).
fn git_reachable_objects(work: &Path) -> BTreeSet<String> {
    git(work, &["rev-list", "--all", "--objects"])
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect()
}

/// Object ids stored in every pack of the clone, read with `git show-index`.
fn packed_objects(work: &Path) -> BTreeSet<String> {
    let pack_dir = git_dir_of(work).join("objects/pack");
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(&pack_dir).expect("pack dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|x| x != "idx") {
            continue;
        }
        let output = Command::new("git")
            .current_dir(work)
            .args(["show-index"])
            .stdin(std::fs::File::open(&path).expect("open idx"))
            .output()
            .expect("git show-index");
        assert!(
            output.status.success(),
            "git show-index {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(oid) = line.split_whitespace().nth(1) {
                out.insert(oid.to_owned());
            }
        }
    }
    out
}

fn loose_object_count(work: &Path) -> usize {
    let objects = git_dir_of(work).join("objects");
    std::fs::read_dir(&objects)
        .expect("objects dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit())
        })
        .map(|e| std::fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0))
        .sum()
}

fn assert_git_fsck_clean(work: &Path) {
    let out = git_output(work, &["fsck", "--full", "--no-dangling"]);
    assert!(
        out.status.success(),
        "git fsck: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn open(work: &Path) -> Repository {
    Repository::discover(Some(work), &RepositoryOptions::empty()).expect("open repo")
}

fn parents_of(work: &Path, commit: ObjectId) -> Vec<String> {
    let src_line = git(work, &["cat-file", "commit", &commit.to_hex()]);
    src_line
        .lines()
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.strip_prefix("parent ").map(str::to_owned))
        .collect()
}

fn grit_fsck(repo: &Repository, opts: &FsckOptions) -> fsck::FsckReport {
    fsck::fsck(repo, opts, &mut NullProgress, &NeverCancel).expect("grit fsck")
}

/// Repack packs the history down to the boundary and nothing below it.
fn assert_repack_stops_at_boundary(depth: usize) {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 5);
    let work = shallow_clone(tmp.path(), &src, "clone", depth, false);
    let boundaries = shallow_boundaries(&work);
    assert_eq!(boundaries.len(), 1, "linear history has one boundary");
    let parents = parents_of(&work, boundaries[0]);
    assert_eq!(parents.len(), 1, "boundary commit records its parent");

    let repo = open(&work);
    let stats = repack_all(&repo, &RepackOptions::default()).expect("repack in shallow clone");
    assert!(stats.objects_packed.is_some(), "{stats:?}");
    clear_pack_cache();

    let packed = packed_objects(&work);
    assert!(
        packed.contains(&boundaries[0].to_hex()),
        "boundary commit packed"
    );
    assert!(
        !packed.contains(&parents[0]),
        "parent below the boundary is absent"
    );
    assert_eq!(packed, git_reachable_objects(&work));
    assert_eq!(loose_object_count(&work), 0);
    assert_git_fsck_clean(&work);
}

#[test]
fn repack_in_depth_one_clone_packs_down_to_the_boundary() {
    assert_repack_stops_at_boundary(1);
}

#[test]
fn repack_in_depth_two_clone_packs_down_to_the_boundary() {
    assert_repack_stops_at_boundary(2);
}

#[test]
fn repack_dry_run_counts_only_objects_above_the_boundary() {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 4);
    let work = shallow_clone(tmp.path(), &src, "clone", 2, false);
    explode_packs(&work);

    let repo = open(&work);
    let mut opts = RepackOptions::default();
    opts.dry_run = true;
    let stats = repack_all(&repo, &opts).expect("dry-run repack");
    assert_eq!(
        stats.objects_packed,
        Some(git_reachable_objects(&work).len())
    );
}

#[test]
fn gc_packs_loose_shallow_clone_and_git_fsck_is_clean() {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 6);
    let work = shallow_clone(tmp.path(), &src, "clone", 2, false);
    explode_packs(&work);
    let reachable = git_reachable_objects(&work);
    assert_eq!(loose_object_count(&work), reachable.len());

    let repo = open(&work);
    let stats = gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false))
        .expect("gc in shallow clone");
    assert!(stats.repack.pack_path.is_some(), "{stats:?}");
    clear_pack_cache();

    assert_eq!(packed_objects(&work), reachable);
    assert_eq!(loose_object_count(&work), 0);
    assert_git_fsck_clean(&work);
}

#[test]
fn gc_in_bare_shallow_clone_writes_bitmap_git_accepts() {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 5);
    let bare = shallow_clone(tmp.path(), &src, "clone.git", 2, true);
    explode_packs(&bare);

    let repo = open(&bare);
    gc(&repo, &GcOptions::with_reflog_policy(1_700_000_000, false))
        .expect("gc in bare shallow clone");
    clear_pack_cache();

    let pack_dir = bare.join("objects/pack");
    let has_bitmap = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|x| x == "bitmap"));
    assert!(has_bitmap, "bare gc writes a reachability bitmap");
    git(&bare, &["rev-list", "--test-bitmap", "HEAD"]);
    let with_bitmap: BTreeSet<String> = git(
        &bare,
        &["rev-list", "--all", "--objects", "--use-bitmap-index"],
    )
    .lines()
    .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
    .collect();
    assert_eq!(with_bitmap, git_reachable_objects(&bare));
    assert_git_fsck_clean(&bare);
}

#[test]
fn prune_keeps_everything_reachable_down_to_the_boundary() {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 5);
    let work = shallow_clone(tmp.path(), &src, "clone", 2, false);
    explode_packs(&work);
    let loose_before = loose_object_count(&work);

    let repo = open(&work);
    let future = SystemTime::now() + Duration::from_secs(3600);
    let mut opts = PruneOptions::default();
    opts.older_than = Some(future);
    let stats = prune_loose_objects(&repo, &opts).expect("prune in shallow clone");
    assert_eq!(stats.pruned, 0, "{stats:?}");
    assert_eq!(loose_object_count(&work), loose_before);

    let head = git_oid(&work, "HEAD");
    let stats =
        prune_loose_unreachable(repo.odb(), &[head], None).expect("odb prune in shallow clone");
    assert_eq!(stats.pruned, 0, "{stats:?}");
    assert_eq!(loose_object_count(&work), loose_before);
    assert_git_fsck_clean(&work);
}

#[test]
fn prune_still_removes_unreachable_loose_objects_in_shallow_clone() {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 3);
    let work = shallow_clone(tmp.path(), &src, "clone", 1, false);
    std::fs::write(work.join("stray.txt"), "unreachable\n").expect("write");
    let stray = git(&work, &["hash-object", "-w", "stray.txt"]);
    let stray = stray.trim().to_owned();

    let repo = open(&work);
    let stats = prune_loose_unreachable(repo.odb(), &[git_oid(&work, "HEAD")], None)
        .expect("prune in shallow clone");
    assert!(stats.pruned >= 1, "{stats:?}");
    let still_there = git_output(&work, &["cat-file", "-e", &stray]);
    assert!(!still_there.status.success(), "stray blob pruned");
    assert_git_fsck_clean(&work);
}

#[test]
fn fsck_reports_no_issues_in_packed_and_loose_shallow_clones() {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 5);
    for depth in [1, 2] {
        let work = shallow_clone(tmp.path(), &src, &format!("clone{depth}"), depth, false);
        for loose in [false, true] {
            if loose {
                explode_packs(&work);
            }
            assert_git_fsck_clean(&work);
            let repo = open(&work);
            for opts in [
                FsckOptions::full_repository(true),
                FsckOptions::connectivity_and_refs(true),
            ] {
                clear_pack_cache();
                let report = grit_fsck(&repo, &opts);
                assert!(
                    report.issues.is_empty(),
                    "depth {depth} loose {loose}: {:?}",
                    report.issues
                );
                assert!(report.dangling.is_empty(), "{:?}", report.dangling);
            }
        }
    }
}

#[test]
fn fsck_still_reports_a_missing_blob_inside_shallow_history() {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 4);
    let work = shallow_clone(tmp.path(), &src, "clone", 2, false);
    explode_packs(&work);
    let boundary = shallow_boundaries(&work)[0];
    let blob = git_oid(&work, &format!("{}:f0.txt", boundary.to_hex()));
    let hex = blob.to_hex();
    std::fs::remove_file(
        git_dir_of(&work)
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]),
    )
    .expect("remove blob");
    assert!(!git_output(&work, &["fsck", "--full"]).status.success());

    let repo = open(&work);
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    let missing: BTreeSet<_> = report
        .issues
        .iter()
        .filter_map(|issue| match issue {
            FsckIssue::MissingObject { oid, .. } => Some(*oid),
            _ => None,
        })
        .collect();
    assert_eq!(missing, BTreeSet::from([blob]), "{:?}", report.issues);
}

#[test]
fn fsck_treats_history_below_boundary_as_unreachable_like_git() {
    let _pack_guard = test_pack_cache_guard();
    let tmp = TempDir::new().expect("tempdir");
    let src = source_repo(tmp.path(), 4);
    let work = tmp.path().join("full");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            src.to_str().expect("utf8"),
            work.to_str().expect("utf8"),
        ],
    );
    // Graft HEAD~1 as a shallow boundary while its parent is still present.
    let boundary = git_oid(&work, "HEAD~1");
    let parent = git_oid(&work, "HEAD~2");
    std::fs::write(
        work.join(".git/shallow"),
        format!("{}\n", boundary.to_hex()),
    )
    .expect("write shallow");

    let git_out = git_output(&work, &["fsck", "--full"]);
    assert!(git_out.status.success());
    let git_text = String::from_utf8_lossy(&git_out.stdout);
    assert!(
        git_text.contains(&format!("dangling commit {}", parent.to_hex())),
        "{git_text}"
    );

    let git_unreachable: BTreeSet<String> = git(&work, &["fsck", "--unreachable", "--no-reflogs"])
        .lines()
        .filter_map(|l| l.strip_prefix("unreachable "))
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
        .collect();
    assert!(git_unreachable.contains(&parent.to_hex()));

    let repo = open(&work);
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    let grit_unreachable: BTreeSet<String> = report
        .dangling
        .iter()
        .map(|(_, oid)| oid.to_hex())
        .collect();
    assert_eq!(grit_unreachable, git_unreachable);
    assert!(report.dangling.contains(&(ObjectKind::Commit, parent)));
}

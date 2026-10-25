//! Partial-clone filters in the upload-pack stream: the objects grit sends for each
//! `--filter` must be exactly the objects system `git pack-objects --revs --filter`
//! sends, with and without pack bitmaps, and the pack must index with
//! `git index-pack` (not `--strict`: a filtered pack is not closed by design).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::pack_objects::{
    build_pack_with_shallow_and_filter, write_upload_pack_stream, PackBuildOptions, PackProgress,
    PackStats,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::ObjectFilter;
use grit_test_support::git;

const FILTERS: &[&str] = &[
    "blob:none",
    "blob:limit=100",
    "tree:0",
    "tree:1",
    "tree:2",
    "tree:3",
    "object:type=blob",
    "object:type=tree",
    "object:type=commit",
    "object:type=tag",
    "combine:tree:2+blob:limit=100",
    "combine:blob:none+object:type=tree",
];

/// Inside `combine:`, git keeps a separate seen set per sub-filter, so a tree that
/// `tree:<n>` first meets too deep is not sent even when it is met again within the
/// depth, depending on traversal order. grit counts each tree at the shallowest depth
/// it is found, so for these specs it may send such trees on top of git's objects.
const GIT_ORDER_DEPENDENT: &[&str] = &["combine:tree:2+blob:limit=100"];

/// History whose trees exercise every filter: blobs on both sides of the size limit,
/// a symlink, nested directories, a directory met at depth 1 in an old commit and at
/// depth 2 in a newer one, a side branch and merge, two annotated tags on one commit,
/// a tag of a tag and a tag of a tree.
fn filter_fixture(dir: &Path) -> PathBuf {
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(work.join("b/c")).unwrap();
    std::fs::write(work.join("top.txt"), "top\n").unwrap();
    std::fs::write(work.join("big.bin"), vec![b'x'; 4000]).unwrap();
    std::fs::write(work.join("b/y.txt"), "y\n").unwrap();
    std::fs::write(work.join("b/c/z.txt"), "z\n").unwrap();
    std::os::unix::fs::symlink("top.txt", work.join("link")).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "one"]);
    std::fs::create_dir_all(work.join("a")).unwrap();
    git(&work, &["mv", "b", "a/b"]);
    std::fs::write(work.join("a/x.txt"), "x\n".repeat(80)).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "two"]);
    git(&work, &["checkout", "-q", "-b", "side"]);
    std::fs::write(work.join("a/b/c/side.txt"), "side\n".repeat(50)).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "side"]);
    git(&work, &["tag", "-a", "-m", "first", "side-a"]);
    git(&work, &["tag", "-a", "-m", "second", "side-b"]);
    git(
        &work,
        &["tag", "-a", "-m", "nested", "side-nested", "side-a"],
    );
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("top.txt"), "top two\n").unwrap();
    git(&work, &["commit", "-q", "-am", "three"]);
    git(&work, &["merge", "-q", "--no-edit", "side"]);
    git(
        &work,
        &["tag", "-a", "-m", "tree", "tree-tag", "HEAD^{tree}"],
    );
    let bare = dir.join("served.git");
    git(
        dir,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

fn rev(repo: &Path, spec: &str) -> ObjectId {
    git(repo, &["rev-parse", spec]).trim().parse().unwrap()
}

/// Object ids in `pack`, indexed by `git index-pack` in a fresh repository.
fn pack_object_ids(scratch: &Path, pack: &[u8]) -> BTreeSet<String> {
    let _ = std::fs::remove_dir_all(scratch);
    git(
        Path::new("/"),
        &["init", "-q", "--bare", scratch.to_str().unwrap()],
    );
    let mut child = Command::new("git")
        .current_dir(scratch)
        .args(["index-pack", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(pack).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "index-pack: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(
        scratch,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// `git pack-objects --revs --filter=<spec>`, using pack bitmaps only with
/// `use_bitmaps`: git's bitmap selection differs from its object walk on some filters
/// (under `tree:0` it keeps a tag's tree that is also a commit's root tree), and grit
/// follows each. grit serves `object:type` from the object walk even with bitmaps.
fn git_filtered_pack(
    repo: &Path,
    wants: &[ObjectId],
    haves: &[ObjectId],
    spec: &str,
    use_bitmaps: bool,
) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args([
            "-c",
            &format!("pack.useBitmaps={use_bitmaps}"),
            "pack-objects",
            "--revs",
            "--stdout",
            "-q",
            &format!("--filter={spec}"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = String::new();
    for w in wants {
        input.push_str(&format!("{w}\n"));
    }
    for h in haves {
        input.push_str(&format!("^{h}\n"));
    }
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git pack-objects --filter={spec}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn grit_filtered_pack(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    filter: &ObjectFilter,
    use_bitmaps: bool,
) -> (Vec<u8>, PackStats) {
    let mut opts = PackBuildOptions::for_serve(None, false, true);
    opts.use_bitmaps = use_bitmaps;
    let empty = std::collections::HashSet::new();
    let mut pack = Vec::new();
    let mut progress = |_: PackProgress| {};
    let stats = write_upload_pack_stream(
        repo,
        wants,
        haves,
        &empty,
        &empty,
        Some(filter),
        &opts,
        &mut progress,
        &mut pack,
    )
    .expect("filtered upload-pack stream");
    (pack, stats)
}

/// With `bitmapped`, `blob:none` must also be served by verbatim pack reuse.
fn assert_filters_match_git(
    bare: &Path,
    scratch: &Path,
    wants: &[ObjectId],
    haves: &[ObjectId],
    bitmapped: bool,
) {
    for use_bitmaps in [false, true] {
        let repo = Repository::open(bare, &RepositoryOptions::empty()).unwrap();
        for spec in FILTERS {
            let filter = ObjectFilter::parse(spec).unwrap();
            let (pack, stats) = grit_filtered_pack(&repo, wants, haves, &filter, use_bitmaps);
            let ours = pack_object_ids(&scratch.join("ours.git"), &pack);
            assert_eq!(
                stats.object_count as usize,
                ours.len(),
                "{spec}: header count"
            );
            if bitmapped && use_bitmaps && *spec == "blob:none" {
                assert!(
                    stats.verbatim_reuse_objects > 0,
                    "blob:none reuses pack entries"
                );
            }
            let theirs = pack_object_ids(
                &scratch.join("theirs.git"),
                &git_filtered_pack(
                    bare,
                    wants,
                    haves,
                    spec,
                    use_bitmaps && !spec.contains("object:type"),
                ),
            );
            if GIT_ORDER_DEPENDENT.contains(spec) {
                assert!(
                    ours.is_superset(&theirs),
                    "--filter={spec}: grit omits objects"
                );
                for extra in ours.difference(&theirs) {
                    let kind = git(bare, &["cat-file", "-t", extra]);
                    assert_eq!(kind.trim(), "tree", "--filter={spec}: extra {extra}");
                }
                continue;
            }
            assert_eq!(
                ours, theirs,
                "--filter={spec} (bitmaps: {use_bitmaps}) sends the objects git sends"
            );
        }
    }
}

#[test]
fn every_filter_sends_the_objects_git_sends() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = filter_fixture(tmp.path());
    let wants: Vec<ObjectId> = [
        "main",
        "side",
        "side-a",
        "side-b",
        "side-nested",
        "tree-tag",
    ]
    .iter()
    .map(|r| rev(&bare, r))
    .collect();
    let scratch = tmp.path().join("scratch");

    assert_filters_match_git(&bare, &scratch, &wants, &[], false);
    git(&bare, &["repack", "-adbq"]);
    assert_filters_match_git(&bare, &scratch, &wants, &[], true);
}

#[test]
fn filters_subtract_the_objects_the_client_has() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = filter_fixture(tmp.path());
    let wants = [rev(&bare, "main"), rev(&bare, "side-b")];
    let haves = [rev(&bare, "main~2")];
    let scratch = tmp.path().join("scratch");
    assert_filters_match_git(&bare, &scratch, &wants, &haves, false);
}

/// Two annotated tags on one commit both travel with the pack; the second used to
/// replace the first, so the client was missing a wanted tag object.
#[test]
fn tags_sharing_a_commit_are_all_sent_with_a_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = filter_fixture(tmp.path());
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).unwrap();
    let tags = [
        rev(&bare, "side-a"),
        rev(&bare, "side-b"),
        rev(&bare, "side-nested"),
    ];
    let filter = ObjectFilter::parse("blob:none").unwrap();
    let (pack, _) = grit_filtered_pack(&repo, &tags, &[], &filter, false);
    let sent = pack_object_ids(&tmp.path().join("check.git"), &pack);
    for tag in tags {
        assert!(sent.contains(&tag.to_hex()), "tag {tag} missing from pack");
    }
}

#[test]
fn in_memory_filtered_pack_matches_git() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = filter_fixture(tmp.path());
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).unwrap();
    let wants = [rev(&bare, "main"), rev(&bare, "side-b")];
    let haves = [rev(&bare, "main~2")];
    let empty = std::collections::HashSet::new();
    for spec in ["blob:none", "tree:1"] {
        let filter = ObjectFilter::parse(spec).unwrap();
        let pack = build_pack_with_shallow_and_filter(
            repo.odb(),
            &wants,
            &haves,
            &empty,
            &empty,
            Some(&filter),
            &PackBuildOptions::for_serve(None, false, true),
        )
        .expect("filtered pack");
        let ours = pack_object_ids(&tmp.path().join("ours.git"), &pack);
        let theirs = pack_object_ids(
            &tmp.path().join("theirs.git"),
            &git_filtered_pack(&bare, &wants, &haves, spec, false),
        );
        assert_eq!(
            ours, theirs,
            "build_pack_with_shallow_and_filter --filter={spec}"
        );
    }
}

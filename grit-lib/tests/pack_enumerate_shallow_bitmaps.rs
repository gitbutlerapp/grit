//! Pack enumeration for a shallow client on a repository with reachability
//! bitmaps: the client's shallow commits bound its haves, so the bitmap closure
//! of a have (which runs to the root) must not decide what the client holds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::pack_object_select::{
    enumerate_pack_objects, HaveBoundary, PackEnumerateOptions,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git(dir: &Path, args: &[&str]) -> String {
    grit_test_support::git(dir, args).trim().to_owned()
}

fn oid(hex: &str) -> ObjectId {
    hex.parse().expect("object id")
}

/// A bare repository of eight linear commits, fully repacked with a bitmap.
fn bitmapped_repo(tmp: &Path) -> std::path::PathBuf {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    for i in 1..=8 {
        std::fs::write(work.join(format!("f{}", i % 3)), format!("{i}\n")).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    let bare = tmp.join("srv.git");
    git(
        tmp,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(&bare, &["repack", "-adbq"]);
    let has_bitmap = std::fs::read_dir(bare.join("objects/pack"))
        .unwrap()
        .any(|e| e.unwrap().path().extension().is_some_and(|x| x == "bitmap"));
    assert!(has_bitmap, "fixture must carry a reachability bitmap");
    bare
}

fn selected(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    use_bitmaps: bool,
    client_shallow: &HashSet<ObjectId>,
) -> HashSet<ObjectId> {
    let no_grafts = HashSet::new();
    enumerate_pack_objects(
        repo.odb(),
        wants,
        haves,
        &PackEnumerateOptions {
            filter: None,
            shallow_grafts: &no_grafts,
            have_shallow_grafts: Some(client_shallow),
            use_bitmaps,
            exclude_objects: None,
            peer_odb: None,
            have_boundary: HaveBoundary::TipsOnly,
        },
    )
    .expect("enumerate")
    .send
    .into_iter()
    .collect()
}

/// `--unshallow` of a depth-1 client: it wants the tip's parent and has only the
/// tip, which is shallow on its side. Everything behind the tip must be sent.
#[test]
fn shallow_have_on_bitmapped_repo_sends_history_behind_the_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = bitmapped_repo(tmp.path());
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open");
    let tip = oid(&git(&bare, &["rev-parse", "main"]));
    let parent = oid(&git(&bare, &["rev-parse", "main^"]));
    let client_shallow: HashSet<ObjectId> = [tip].into_iter().collect();

    let with_bitmaps = selected(&repo, &[tip, parent], &[tip], true, &client_shallow);
    let walked = selected(&repo, &[tip, parent], &[tip], false, &client_shallow);
    assert_eq!(
        with_bitmaps, walked,
        "bitmaps must not change the selection"
    );

    let mut expected: HashSet<ObjectId> = git(&bare, &["rev-list", "--objects", "main^"])
        .lines()
        .map(|l| oid(l.split(' ').next().unwrap()))
        .collect();
    for held in git(&bare, &["rev-list", "--objects", "--no-walk", "main"]).lines() {
        expected.remove(&oid(held.split(' ').next().unwrap()));
    }
    for held in git(&bare, &["ls-tree", "-r", "-t", "--object-only", "main"]).lines() {
        expected.remove(&oid(held));
    }
    assert!(with_bitmaps.contains(&parent), "the parent commit is sent");
    assert_eq!(with_bitmaps, expected);
}

/// Without client shallow commits a have's whole history is held, and bitmaps
/// stay in use with the same answer as the walk.
#[test]
fn full_client_have_on_bitmapped_repo_matches_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = bitmapped_repo(tmp.path());
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open");
    let tip = oid(&git(&bare, &["rev-parse", "main"]));
    let base = oid(&git(&bare, &["rev-parse", "main~3"]));
    let none = HashSet::new();

    let with_bitmaps = selected(&repo, &[tip], &[base], true, &none);
    let walked = selected(&repo, &[tip], &[base], false, &none);
    assert_eq!(with_bitmaps, walked);
    let expected: HashSet<ObjectId> = git(&bare, &["rev-list", "--objects", "main", "^main~3"])
        .lines()
        .map(|l| oid(l.split(' ').next().unwrap()))
        .collect();
    assert_eq!(with_bitmaps, expected);
}

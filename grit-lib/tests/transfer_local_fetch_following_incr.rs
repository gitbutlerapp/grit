//! Incremental local fetch with default `TagMode::Following`.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

#[test]
fn fetch_local_incremental_following_imports_reachable_tag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    std::fs::write(remote.join("README"), b"incremental tag\n").unwrap();
    git(&remote, &["add", "README"]);
    git(&remote, &["commit", "-q", "-m", "init"]);

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let remote_git = remote.join(".git");

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_local(&local_git, &remote_git, &opts).expect("initial fetch");

    std::fs::write(remote.join("next.txt"), b"second\n").unwrap();
    git(&remote, &["add", "next.txt"]);
    git(&remote, &["commit", "-q", "-m", "second"]);
    git(&remote, &["tag", "-a", "v2", "-m", "after second"]);

    let outcome = fetch_local(&local_git, &remote_git, &opts).expect("incremental fetch");

    let tag_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/tags/v2")
        .expect("tag update in outcome");
    assert_eq!(
        tag_update.mode,
        UpdateMode::New,
        "incremental Following fetch must import newly reachable tag"
    );

    let tag_oid = rev_parse(&remote, "refs/tags/v2");
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/v2").expect("tag ref"),
        tag_oid
    );

    let out = Command::new("git")
        .args(["cat-file", "-e", &tag_oid.to_hex()])
        .current_dir(&local)
        .env("GIT_DIR", &local_git)
        .output()
        .expect("cat-file");
    assert!(out.status.success(), "tag object must be present locally");
}

/// Incremental `Following` fetch into a repo that already holds most remote tags:
/// only the new reachable tag is imported, unchanged tags are not reported as
/// modified, a moved tag keeps its local value, an unreachable tag stays out, and
/// the final tag set matches what the system `git fetch` produces.
#[test]
fn fetch_local_incremental_following_with_existing_tags_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    for i in 0..3 {
        std::fs::write(remote.join("f.txt"), format!("{i}\n")).unwrap();
        git(&remote, &["add", "f.txt"]);
        git(&remote, &["commit", "-q", "-m", &format!("c{i}")]);
        git(&remote, &["tag", "-a", &format!("v{i}"), "-m", "release"]);
        git(&remote, &["tag", &format!("light{i}")]);
    }
    git(&remote, &["tag", "moving"]);
    git(&remote, &["checkout", "-q", "-b", "side"]);
    git(&remote, &["commit", "-q", "--allow-empty", "-m", "side"]);
    git(&remote, &["tag", "side-only"]);
    git(&remote, &["checkout", "-q", "main"]);

    let refspec = "+refs/heads/main:refs/remotes/origin/main";
    let remote_git = remote.join(".git");
    let remote_url = remote_git.to_str().unwrap();
    let local = tmp.path().join("local");
    let reference = tmp.path().join("reference");
    for dir in [&local, &reference] {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main", "."]);
    }
    let local_git = local.join(".git");
    let opts = FetchOptions {
        refspecs: vec![refspec.to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_local(&local_git, &remote_git, &opts).expect("initial fetch");
    git(&reference, &["fetch", "-q", remote_url, refspec]);
    let moving_before = rev_parse(&local, "refs/tags/moving");

    git(&remote, &["commit", "-q", "--allow-empty", "-m", "next"]);
    git(&remote, &["tag", "-a", "v3", "-m", "release three"]);
    git(&remote, &["tag", "-f", "moving"]);

    let outcome = fetch_local(&local_git, &remote_git, &opts).expect("incremental fetch");
    git(&reference, &["fetch", "-q", remote_url, refspec]);

    let v3 = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/tags/v3")
        .expect("v3 in outcome");
    assert_eq!(v3.mode, UpdateMode::New);
    for unchanged in ["v0", "v1", "v2", "light0", "light2"] {
        let name = format!("refs/tags/{unchanged}");
        assert!(
            outcome
                .updates
                .iter()
                .filter(|u| u.remote_ref == name)
                .all(|u| !u.mode.modified_repo()),
            "{name} must not be reported as modified"
        );
    }
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/moving").expect("moving tag"),
        moving_before,
        "an existing tag must not move without force"
    );
    assert!(resolve_ref(&local_git, "refs/tags/side-only").is_err());
    assert_eq!(
        git(&local, &["for-each-ref", "refs/tags"]),
        git(&reference, &["for-each-ref", "refs/tags"]),
        "grit and git must follow the same tags"
    );
    let fsck = Command::new("git")
        .args(["fsck", "--no-dangling"])
        .current_dir(&local)
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

/// After both sides already have `main`, a newly advertised lightweight tag at the
/// existing tip must be imported (Git `fetch` creates `refs/tags/*`; grit must match).
#[test]
fn fetch_local_incremental_following_imports_lightweight_tag_on_existing_tip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    std::fs::write(remote.join("README"), b"tip tag\n").unwrap();
    git(&remote, &["add", "README"]);
    git(&remote, &["commit", "-q", "-m", "init"]);

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let remote_git = remote.join(".git");

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_local(&local_git, &remote_git, &opts).expect("initial fetch");

    git(&remote, &["tag", "lightweight-on-existing-tip"]);

    let outcome = fetch_local(&local_git, &remote_git, &opts).expect("noop-object fetch");

    let tag_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/tags/lightweight-on-existing-tip")
        .expect("lightweight tag update");
    assert_eq!(tag_update.mode, UpdateMode::New);

    let tip = rev_parse(&remote, "refs/heads/main");
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/lightweight-on-existing-tip").expect("tag ref"),
        tip,
        "lightweight tag must point at the already-local commit"
    );
}

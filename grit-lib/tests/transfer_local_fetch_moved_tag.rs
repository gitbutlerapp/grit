//! Default tag following must not treat a moved remote tag as a rejection.

use grit_test_support::rev_parse;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::remote::transfer::{FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn following_remote_heads() -> FetchOptions {
    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.tags = TagMode::Following;
    opts
}

#[test]
fn fetch_local_following_ignores_moved_tag_without_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    std::fs::write(remote.join("a.txt"), b"a\n").unwrap();
    git(&remote, &["add", "a.txt"]);
    git(&remote, &["commit", "-q", "-m", "a"]);
    git(&remote, &["tag", "newtag"]);

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let remote_git = remote.join(".git");

    let opts = following_remote_heads();
    grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&remote_git),
        &mut opts.clone(),
        &mut grit_lib::progress::NullProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("initial fetch");

    let tag_before = grit_test_support::refs::at(&local_git)
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/tags/newtag").expect("valid ref name"),
        )
        .expect("tag after first fetch");
    assert_eq!(tag_before, rev_parse(&remote, "refs/tags/newtag"));

    std::fs::write(remote.join("b.txt"), b"b\n").unwrap();
    git(&remote, &["add", "b.txt"]);
    git(&remote, &["commit", "-q", "-m", "b"]);
    git(&remote, &["tag", "-f", "newtag"]);

    let outcome = grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&remote_git),
        &mut opts.clone(),
        &mut grit_lib::progress::NullProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("fetch after tag move");
    assert!(
        !outcome.has_rejections(),
        "Following must not reject a tag that already exists locally: {:?}",
        outcome.updates
    );
    assert!(
        outcome
            .updates
            .iter()
            .all(|u| u.remote_ref != "refs/tags/newtag"),
        "moved tag must not appear in updates under Following"
    );

    assert_eq!(
        grit_test_support::refs::at(&local_git)
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/tags/newtag").expect("valid ref name")
            )
            .expect("local tag unchanged"),
        tag_before,
        "local tag must not move without --tags --force"
    );

    let outcome2 = grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&remote_git),
        &mut opts.clone(),
        &mut grit_lib::progress::NullProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("repeat fetch");
    assert!(!outcome2.has_rejections());
    assert!(outcome2
        .updates
        .iter()
        .all(|u| u.remote_ref != "refs/tags/newtag"));

    let out = Command::new("git")
        .args(["rev-parse", "newtag"])
        .current_dir(&local)
        .output()
        .expect("git rev-parse");
    assert!(out.status.success());
    let git_tag =
        ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("git tag oid");
    assert_eq!(git_tag, tag_before);
}

#[test]
fn fetch_local_all_tags_rejects_moved_tag_until_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    std::fs::write(remote.join("a.txt"), b"a\n").unwrap();
    git(&remote, &["add", "a.txt"]);
    git(&remote, &["commit", "-q", "-m", "a"]);
    git(&remote, &["tag", "newtag"]);

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let remote_git = remote.join(".git");

    let base = following_remote_heads();
    grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&base).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&remote_git),
        &mut base.clone(),
        &mut grit_lib::progress::NullProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("seed");

    let tag_before = grit_test_support::refs::at(&local_git)
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/tags/newtag").expect("valid ref name"),
        )
        .unwrap();
    std::fs::write(remote.join("b.txt"), b"b\n").unwrap();
    git(&remote, &["add", "b.txt"]);
    git(&remote, &["commit", "-q", "-m", "b"]);
    git(&remote, &["tag", "-f", "newtag"]);
    let tag_after = rev_parse(&remote, "refs/tags/newtag");

    let mut all_tags = base.clone();
    all_tags.tags = TagMode::All;
    let rejected = grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&all_tags).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&remote_git),
        &mut all_tags.clone(),
        &mut grit_lib::progress::NullProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("fetch --tags");
    assert!(rejected.has_rejections());
    let tag_update = rejected
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/tags/newtag")
        .expect("moved tag reported under All");
    assert_eq!(tag_update.mode, UpdateMode::TagUpdateRejected);
    assert_eq!(
        grit_test_support::refs::at(&local_git)
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/tags/newtag").expect("valid ref name")
            )
            .unwrap(),
        tag_before
    );

    let mut forced = all_tags;
    forced.force = true;
    let applied = grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&forced).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&remote_git),
        &mut forced.clone(),
        &mut grit_lib::progress::NullProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("fetch --tags --force");
    assert!(!applied.has_rejections());
    assert_eq!(
        grit_test_support::refs::at(&local_git)
            .references()
            .resolve(
                grit_lib::refs::store::RefNameRef::new("refs/tags/newtag").expect("valid ref name")
            )
            .unwrap(),
        tag_after
    );

    git(&local, &["fsck", "--strict", "--no-dangling"]);
}

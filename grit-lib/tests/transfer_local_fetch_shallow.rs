//! Shallow-clone fetch/clone via [`grit_lib::remote::transfer::fetch_local`].
//!
//! Reproduces GitHub issue #937: fetch in a `git clone --depth 1` checkout must
//! not create tag refs to missing commits, and cloning from a shallow source must
//! propagate `.git/shallow` and stay fsck-clean with system `git`.

use grit_test_support::rev_parse;
use std::path::Path;
use std::process::Command;

use grit_lib::refs::resolve_ref;
use grit_lib::remote::transfer::{fetch_local, FetchDepth, FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn git_fsck_clean(work_tree: &Path) {
    let out = Command::new("git")
        .current_dir(work_tree)
        .args(["fsck"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn build_tagged_linear_source(bare: &Path) {
    let wt = bare.with_extension("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q", "-b", "main", "."]);
    for i in 1..=3 {
        std::fs::write(wt.join("f"), format!("{i}\n")).unwrap();
        git(&wt, &["add", "f"]);
        git(&wt, &["commit", "-q", "-m", &format!("c{i}")]);
        git(&wt, &["tag", &format!("v{i}")]);
    }
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
}

#[test]
fn fetch_local_in_git_shallow_clone_does_not_create_broken_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_tagged_linear_source(&bare);

    let shallow = tmp.path().join("shallow");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", bare.display()),
            shallow.to_str().unwrap(),
        ],
    );

    let shallow_git = shallow.join(".git");
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_local(&shallow_git, &bare, &opts).expect("fetch_local");

    assert!(
        resolve_ref(&shallow_git, "refs/tags/v1").is_err(),
        "v1 must not be created in a depth-1 shallow clone"
    );
    assert!(
        resolve_ref(&shallow_git, "refs/tags/v2").is_err(),
        "v2 must not be created in a depth-1 shallow clone"
    );
    assert_eq!(
        resolve_ref(&shallow_git, "refs/tags/v3").expect("v3"),
        rev_parse(&shallow, "HEAD")
    );

    git_fsck_clean(&shallow);
}

#[test]
fn fetch_local_clone_from_shallow_source_propagates_shallow_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    build_tagged_linear_source(&bare);

    let shallow_src = tmp.path().join("shallow-src");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", bare.display()),
            shallow_src.to_str().unwrap(),
        ],
    );
    assert!(
        shallow_src.join(".git/shallow").is_file(),
        "git shallow clone must have .git/shallow"
    );

    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    git(&dest, &["init", "-q", "-b", "main", "."]);
    let dest_git = dest.join(".git");

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        initial_remote_fetch: true,
        remote_name: Some("origin".to_owned()),
        ..Default::default()
    };
    fetch_local(&dest_git, &shallow_src.join(".git"), &opts).expect("fetch_local from shallow");

    assert!(
        dest_git.join("shallow").is_file(),
        "clone fetch must copy shallow boundaries from a shallow source"
    );
    assert_eq!(
        resolve_ref(&dest_git, "refs/tags/v3").expect("v3 on shallow clone"),
        rev_parse(&shallow_src, "HEAD")
    );
    assert!(
        resolve_ref(&dest_git, "refs/tags/v1").is_err(),
        "v1 must not be imported from a depth-1 source"
    );
    let log = git(&dest, &["log", "--oneline", "refs/remotes/origin/main"]);
    assert_eq!(log.lines().count(), 1, "expected single commit history");
    git_fsck_clean(&dest);
}

#[test]
fn fetch_local_initial_fetch_following_imports_reachable_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main", "."]);
    std::fs::write(upstream.join("README"), b"tag following\n").unwrap();
    git(&upstream, &["add", "README"]);
    git(&upstream, &["commit", "-q", "-m", "init"]);
    git(&upstream, &["tag", "-a", "v1.0", "-m", "release"]);
    git(&upstream, &["tag", "lightweight-tip"]);
    git(&upstream, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(upstream.join("topic.txt"), b"topic\n").unwrap();
    git(&upstream, &["add", "topic.txt"]);
    git(&upstream, &["commit", "-q", "-m", "topic work"]);
    git(&upstream, &["checkout", "-q", "main"]);
    git(
        &upstream,
        &["tag", "-a", "on-topic", "-m", "topic tag", "topic"],
    );

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let upstream_git = upstream.join(".git");

    fetch_local(
        &local_git,
        &upstream_git,
        &FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::Following,
            initial_remote_fetch: true,
            remote_name: Some("origin".to_owned()),
            ..Default::default()
        },
    )
    .expect("initial fetch");

    for tag in [
        "refs/tags/v1.0",
        "refs/tags/lightweight-tip",
        "refs/tags/on-topic",
    ] {
        resolve_ref(&local_git, tag).unwrap_or_else(|_| panic!("missing {tag}"));
    }
    git_fsck_clean(&local);
}

fn build_historical_tag_source(bare: &Path) {
    build_tagged_linear_source(bare);
    git(bare, &["tag", "-d", "v1", "v3"]);
    git(bare, &["tag", "-a", "v1", "-m", "release", "main~2"]);
}

fn depth_one_client(base: &Path, source: &Path, name: &str) -> std::path::PathBuf {
    let client = base.join(name);
    git(
        base,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", source.display()),
            client.to_str().unwrap(),
        ],
    );
    assert!(git(&client, &["tag", "--list"]).trim().is_empty());
    client
}

fn assert_tag_history_matches_git(client: &Path, comparison: &Path) {
    assert_eq!(
        git(
            client,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname) %(*objectname)",
                "refs/tags/"
            ]
        ),
        git(
            comparison,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname) %(*objectname)",
                "refs/tags/"
            ]
        ),
        "tag names, object IDs and peeled IDs must match after one fetch",
    );
    assert_eq!(
        git(client, &["rev-list", "HEAD"]),
        git(comparison, &["rev-list", "HEAD"]),
    );
    assert_eq!(
        std::fs::read(client.join(".git/shallow")).ok(),
        std::fs::read(comparison.join(".git/shallow")).ok(),
    );
    git(client, &["fsck", "--strict"]);
    git(comparison, &["fsck", "--strict"]);
}

#[test]
fn unshallow_follows_historical_tags_in_one_fetch() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source.git");
    build_historical_tag_source(&source);
    let client = depth_one_client(tmp.path(), &source, "client");
    let comparison = depth_one_client(tmp.path(), &source, "comparison");
    git(&comparison, &["fetch", "--unshallow"]);
    let outcome = fetch_local(
        &client.join(".git"),
        &source,
        &FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
            tags: TagMode::Following,
            fetch_depth: Some(FetchDepth::Unshallow),
            ..Default::default()
        },
    )
    .unwrap();
    assert_tag_history_matches_git(&client, &comparison);
    assert_eq!(git(&client, &["tag", "--list"]).trim(), "v1\nv2");
    assert_eq!(git(&client, &["rev-list", "--count", "HEAD"]).trim(), "3");
    assert!(!client.join(".git/shallow").exists());
    for name in ["refs/tags/v1", "refs/tags/v2"] {
        let update = outcome
            .updates
            .iter()
            .find(|u| u.remote_ref == name)
            .unwrap();
        assert_eq!(update.mode, UpdateMode::New);
        assert_eq!(update.old_oid, None);
        assert_eq!(update.new_oid, Some(rev_parse(&source, name)));
    }
}

#[test]
fn shallow_tag_following_uses_requested_history_and_tag_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source.git");
    build_historical_tag_source(&source);
    // A tag on side history must not deepen the selected main branch.
    let seed = source.with_extension("wt");
    git(&seed, &["checkout", "-q", "-b", "side", "main~2"]);
    std::fs::write(seed.join("side"), "side history\n").unwrap();
    git(&seed, &["add", "side"]);
    git(&seed, &["commit", "-q", "-m", "side"]);
    git(&seed, &["tag", "-a", "side-tag", "-m", "side release"]);
    git(
        &seed,
        &["push", source.to_str().unwrap(), "refs/tags/side-tag"],
    );
    // Also exercise a chain of annotated tags.
    git(
        &source,
        &[
            "-c",
            "advice.nestedTag=false",
            "tag",
            "-a",
            "nested",
            "-m",
            "nested release",
            "v1",
        ],
    );
    let cases = [
        (None, TagMode::Following, vec![]),
        (
            FetchDepth::depth(1),
            TagMode::Following,
            vec!["--depth", "1"],
        ),
        (
            FetchDepth::depth(2),
            TagMode::Following,
            vec!["--depth", "2"],
        ),
        (
            FetchDepth::deepen(1),
            TagMode::Following,
            vec!["--deepen", "1"],
        ),
        (
            Some(FetchDepth::Unshallow),
            TagMode::Following,
            vec!["--unshallow"],
        ),
        (
            Some(FetchDepth::Unshallow),
            TagMode::None,
            vec!["--unshallow", "--no-tags"],
        ),
        (
            Some(FetchDepth::Unshallow),
            TagMode::All,
            vec!["--unshallow", "--tags"],
        ),
    ];
    for (i, (fetch_depth, tags, args)) in cases.into_iter().enumerate() {
        let client = depth_one_client(tmp.path(), &source, &format!("client-{i}"));
        let comparison = depth_one_client(tmp.path(), &source, &format!("comparison-{i}"));
        let mut git_args = vec!["fetch"];
        git_args.extend(args);
        git(&comparison, &git_args);
        fetch_local(
            &client.join(".git"),
            &source,
            &FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
                fetch_depth,
                tags,
                ..Default::default()
            },
        )
        .unwrap();
        assert_tag_history_matches_git(&client, &comparison);
        if tags == TagMode::Following {
            assert!(resolve_ref(&client.join(".git"), "refs/tags/side-tag").is_err());
        }
    }
}

#[test]
fn unshallow_follows_tags_only_as_far_as_shallow_source_can_supply() {
    let tmp = tempfile::tempdir().unwrap();
    let complete = tmp.path().join("complete.git");
    build_historical_tag_source(&complete);
    let source = tmp.path().join("source.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            "--depth",
            "2",
            &format!("file://{}", complete.display()),
            source.to_str().unwrap(),
        ],
    );
    let client = depth_one_client(tmp.path(), &source, "client");
    let comparison = depth_one_client(tmp.path(), &source, "comparison");
    git(&comparison, &["fetch", "--unshallow"]);
    fetch_local(
        &client.join(".git"),
        &source,
        &FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".into()],
            tags: TagMode::Following,
            fetch_depth: Some(FetchDepth::Unshallow),
            ..Default::default()
        },
    )
    .unwrap();
    assert_tag_history_matches_git(&client, &comparison);
    assert_eq!(git(&client, &["tag", "--list"]).trim(), "v2");
    assert_eq!(git(&client, &["rev-list", "--count", "HEAD"]).trim(), "2");
    assert!(client.join(".git/shallow").exists());
}

#[test]
fn unshallow_preserves_negative_refspecs_and_existing_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source.git");
    build_historical_tag_source(&source);
    for existing in [false, true] {
        let client = depth_one_client(tmp.path(), &source, &format!("client-{existing}"));
        let mut refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".into()];
        if existing {
            git(&client, &["tag", "v1", "HEAD"]);
            git(&client, &["tag", "shared-tip", "HEAD"]);
            git(&source, &["tag", "shared-tip", "main"]);
        } else {
            refspecs.push("^refs/tags/v1".into());
        }
        let outcome = fetch_local(
            &client.join(".git"),
            &source,
            &FetchOptions {
                refspecs,
                tags: TagMode::Following,
                fetch_depth: Some(FetchDepth::Unshallow),
                ..Default::default()
            },
        )
        .unwrap();
        if existing {
            assert!(
                !outcome.has_rejections(),
                "Following must not reject an existing local tag: {:?}",
                outcome.updates
            );
            assert!(
                outcome
                    .updates
                    .iter()
                    .all(|u| u.remote_ref != "refs/tags/v1"),
                "unchanged local tag must not appear in updates"
            );
            assert!(!outcome
                .updates
                .iter()
                .any(|u| u.remote_ref == "refs/tags/shared-tip"));
            assert_eq!(
                rev_parse(&client, "refs/tags/v1"),
                rev_parse(&client, "HEAD")
            );
        } else {
            assert!(!outcome
                .updates
                .iter()
                .any(|u| u.remote_ref == "refs/tags/v1"));
            assert!(resolve_ref(&client.join(".git"), "refs/tags/v1").is_err());
        }
        assert_eq!(
            rev_parse(&client, "refs/tags/v2"),
            rev_parse(&source, "refs/tags/v2")
        );
        git(&client, &["fsck", "--strict"]);
    }
}

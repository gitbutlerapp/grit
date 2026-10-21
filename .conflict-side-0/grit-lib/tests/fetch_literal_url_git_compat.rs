//! Anonymous URL/path fetch must not rewrite configured `refs/remotes/origin/*`.

use grit_lib::refs::resolve_ref;
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode};
use grit_lib::remote::Remote;
use grit_test_support::git;

fn origin_tip(path: &std::path::Path) -> grit_lib::objects::ObjectId {
    resolve_ref(&path.join(".git"), "refs/remotes/origin/main").expect("origin/main")
}

#[test]
fn fetch_literal_path_leaves_origin_tracking_refs_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let o1 = tmp.path().join("o1");
    let o2 = tmp.path().join("o2");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", o1.to_str().unwrap()],
    );
    git(&o1, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", o2.to_str().unwrap()],
    );
    git(&o2, &["commit", "-q", "--allow-empty", "-m", "two"]);
    git(&o2, &["branch", "otherbr"]);

    let clone = tmp.path().join("c");
    git(
        tmp.path(),
        &["clone", "-q", o1.to_str().unwrap(), clone.to_str().unwrap()],
    );
    let local_git = clone.join(".git");
    let origin_before = origin_tip(&clone);
    let o1_main = resolve_ref(&o1.join(".git"), "refs/heads/main").expect("o1 main");

    fetch_local(
        &local_git,
        &o2.join(".git"),
        &FetchOptions {
            refspecs: vec![Remote::ANONYMOUS_FETCH_REFSPEC.to_owned()],
            tags: TagMode::None,
            fetch_source: Some(o2.to_str().unwrap().to_owned()),
            ..Default::default()
        },
    )
    .expect("fetch o2");

    assert_eq!(origin_tip(&clone), origin_before);
    assert_eq!(origin_tip(&clone), o1_main);
    assert!(
        resolve_ref(&local_git, "refs/remotes/origin/otherbr").is_err(),
        "must not create origin/otherbr from foreign repo"
    );

    let grit_fetch_head =
        std::fs::read_to_string(local_git.join("FETCH_HEAD")).expect("FETCH_HEAD");
    let grit_oid = grit_fetch_head
        .split('\t')
        .next()
        .expect("oid field")
        .trim();
    let o2_main = resolve_ref(&o2.join(".git"), "refs/heads/main").expect("o2 main");
    assert_eq!(grit_oid, o2_main.to_hex());

    let git_fetch = tmp.path().join("git-c");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            o1.to_str().unwrap(),
            git_fetch.to_str().unwrap(),
        ],
    );
    git(&git_fetch, &["fetch", o2.to_str().unwrap()]);
    let git_fetch_head =
        std::fs::read_to_string(git_fetch.join(".git/FETCH_HEAD")).expect("git FETCH_HEAD");
    let git_oid = git_fetch_head
        .split('\t')
        .next()
        .expect("git oid field")
        .trim();
    assert_eq!(grit_oid, git_oid);
    assert_eq!(
        origin_tip(&git_fetch),
        origin_before,
        "git baseline: origin/main unchanged"
    );
}

#[test]
fn explicit_url_refspec_fetch_head_records_fetched_branch_not_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let o1 = tmp.path().join("o1");
    let o2 = tmp.path().join("o2");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", o1.to_str().unwrap()],
    );
    git(&o1, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", o2.to_str().unwrap()],
    );
    git(&o2, &["commit", "-q", "--allow-empty", "-m", "main-tip"]);
    git(&o2, &["branch", "otherbr"]);
    git(&o2, &["commit", "-q", "--allow-empty", "-m", "other-tip"]);

    let clone = tmp.path().join("c");
    git(
        tmp.path(),
        &["clone", "-q", o1.to_str().unwrap(), clone.to_str().unwrap()],
    );
    let local_git = clone.join(".git");
    let o2_path = o2.to_str().unwrap();

    fetch_local(
        &local_git,
        &o2.join(".git"),
        &FetchOptions {
            refspecs: vec![format!("refs/heads/otherbr:refs/remotes/foreign/otherbr")],
            tags: TagMode::None,
            fetch_source: Some(o2_path.to_owned()),
            ..Default::default()
        },
    )
    .expect("fetch otherbr");

    let other_tip = resolve_ref(&o2.join(".git"), "refs/heads/otherbr").expect("otherbr tip");
    let main_tip = resolve_ref(&o2.join(".git"), "refs/heads/main").expect("main tip");
    assert_ne!(other_tip, main_tip);

    let grit_fetch_head =
        std::fs::read_to_string(local_git.join("FETCH_HEAD")).expect("FETCH_HEAD");
    let grit_oid = grit_fetch_head
        .split('\t')
        .next()
        .expect("oid field")
        .trim();
    assert_eq!(grit_oid, other_tip.to_hex());
    assert!(
        grit_fetch_head.contains("branch 'otherbr' of"),
        "expected branch description in FETCH_HEAD: {grit_fetch_head}"
    );

    let git_fetch = tmp.path().join("git-c");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            o1.to_str().unwrap(),
            git_fetch.to_str().unwrap(),
        ],
    );
    git(
        &git_fetch,
        &[
            "fetch",
            o2_path,
            "refs/heads/otherbr:refs/remotes/foreign/otherbr",
        ],
    );
    let git_fetch_head =
        std::fs::read_to_string(git_fetch.join(".git/FETCH_HEAD")).expect("git FETCH_HEAD");
    let git_oid = git_fetch_head
        .split('\t')
        .next()
        .expect("git oid field")
        .trim();
    assert_eq!(grit_oid, git_oid);
}

#[test]
fn anonymous_fetch_refspec_is_empty_destination() {
    let remote = Remote::from_url("/tmp/upstream").expect("url");
    assert_eq!(
        remote.fetch_refspecs,
        vec![Remote::ANONYMOUS_FETCH_REFSPEC.to_owned()]
    );
    assert!(remote.name.is_none());
}

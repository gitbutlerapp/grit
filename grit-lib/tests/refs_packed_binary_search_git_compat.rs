//! Large git-`pack-refs` repositories: grit resolves every packed ref like `git rev-parse`.

use crate::support;

use grit_lib::refs::{resolve_ref, PackedRefs};

use support::{files_repo, git, git_empty_commit_oid, git_for_each_ref, TestRepo};

fn git_dir(repo: &TestRepo) -> std::path::PathBuf {
    repo.git_dir()
}

#[test]
fn git_packed_two_thousand_tags_resolve_matches_git() {
    let repo = files_repo();
    let wt = repo.worktree();
    let git_dir = git_dir(&repo);
    git_empty_commit_oid(wt);
    for i in 0..2000 {
        git(
            wt,
            &[
                "tag",
                "-a",
                &format!("t{i:04}"),
                "-m",
                "annotated tag for packed-refs binary search",
            ],
        );
    }
    git(wt, &["pack-refs", "--all"]);

    let header = std::fs::read_to_string(git_dir.join("packed-refs")).expect("packed-refs");
    assert!(
        header.contains("sorted"),
        "git pack-refs --all must write sorted trait"
    );

    let packed = PackedRefs::load(&git_dir).expect("load packed snapshot");
    assert!(packed.get("refs/heads/main").is_some());

    for (name, grit_oid) in git_for_each_ref(wt, "refs/") {
        let git_oid = git(wt, &["rev-parse", &name]);
        assert_eq!(grit_oid.to_hex(), git_oid.trim(), "list parity for {name}");
        assert_eq!(
            resolve_ref(&git_dir, &name).expect("resolve").to_hex(),
            git_oid.trim()
        );
        if let Some(want) = packed.get(&name) {
            assert_eq!(want, grit_oid, "packed snapshot for {name}");
        }
    }
}

#[test]
fn separate_repository_reads_see_git_packed_ref_update() {
    let repo = files_repo();
    let wt = repo.worktree();
    let git_dir = git_dir(&repo);
    git_empty_commit_oid(wt);
    git(wt, &["branch", "session-a"]);
    git(wt, &["pack-refs", "--all"]);

    let oid_before = resolve_ref(&git_dir, "refs/heads/session-a").expect("session-a");
    assert_eq!(
        resolve_ref(&git_dir, "refs/heads/session-a").expect("lookup"),
        oid_before
    );

    git(wt, &["branch", "session-b"]);
    git(wt, &["pack-refs", "--all"]);
    let oid_b = resolve_ref(&git_dir, "refs/heads/session-b").expect("session-b");

    assert_eq!(
        resolve_ref(&git_dir, "refs/heads/session-b").expect("lookup"),
        oid_b
    );
}

//! Smoke test for the refs backend harness.

mod support;

use grit_lib::refs::write_ref;

use support::{each_backend, git_empty_commit_oid, git_show_ref};

#[test]
fn each_backend_inits_and_round_trips_one_ref() {
    each_backend(|_backend, repo| {
        let git_dir = repo.git_dir();
        let oid = git_empty_commit_oid(repo.worktree());
        write_ref(&git_dir, "refs/heads/harness-smoke", &oid).expect("write_ref");

        let grit_map = support::grit_refs(repo.worktree());
        assert_eq!(
            grit_map.get("refs/heads/harness-smoke"),
            Some(&oid),
            "grit list_refs missing smoke ref"
        );

        let git_map = git_show_ref(repo.worktree());
        assert_eq!(
            git_map.get("refs/heads/harness-smoke"),
            Some(&oid),
            "git show-ref missing smoke ref"
        );
    });
}

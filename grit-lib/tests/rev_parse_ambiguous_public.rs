//! Exercise ambiguous short-hex disambiguation through public rev-parse entry points.

use crate::support;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    resolve_reflog_walk_log_ref, resolve_revision, resolve_revision_as_commit,
    resolve_revision_for_range_end, resolve_revision_for_verify,
};

use support::{bare_repo_with_ambiguous_prefix, git, HistoryRepo};

#[test]
fn ambiguous_short_hex_public_resolvers() {
    let (_dir, repo, prefix) = bare_repo_with_ambiguous_prefix(8_000);
    let peel = format!("{prefix}^{{commit}}");
    let _ = resolve_revision_for_verify(&repo, &prefix);
    let _ = resolve_revision_for_range_end(&repo, &prefix);
    let _ = resolve_revision_as_commit(&repo, &prefix);
    let _ = resolve_revision_for_verify(&repo, &peel);
    let _ = resolve_revision(&repo, &prefix);
}

#[test]
fn core_disambiguate_config_values() {
    let (_dir, repo, prefix) = bare_repo_with_ambiguous_prefix(8_000);
    for disambig in [
        "commit",
        "tree",
        "blob",
        "tag",
        "committish",
        "none",
        "invalid",
    ] {
        std::fs::write(
            repo.git_dir().join("config"),
            format!("[core]\n\tdisambiguate = {disambig}\n"),
        )
        .expect("config");
        let _ = resolve_revision(&repo, &prefix);
        let _ = resolve_revision_for_verify(&repo, &prefix);
    }
}

#[test]
fn reflog_walk_ref_at_minus_branch_form() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    support::commit_file(wt, "f.txt", "1\n", "one");
    git(wt, &["checkout", "-b", "first"]);
    support::commit_file(wt, "f.txt", "2\n", "two");
    git(wt, &["checkout", "main"]);
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    let repo = Repository::discover(Some(wt), &options).expect("discover");
    let name = resolve_reflog_walk_log_ref(&repo, "@{-1}").expect("walk ref");
    assert!(name.contains("first") || name.starts_with("refs/"));
}

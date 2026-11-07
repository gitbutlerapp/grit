//! Exercise ambiguous short-hex disambiguation through public rev-parse entry points.

use crate::support;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;

use support::{bare_repo_with_ambiguous_prefix, git, HistoryRepo};

#[test]
fn ambiguous_short_hex_public_resolvers() {
    let (_dir, repo, prefix) = bare_repo_with_ambiguous_prefix(8_000);
    let peel = format!("{prefix}^{{commit}}");
    let _ = repo.revwalk().resolve(
        &prefix,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo.revwalk().resolve(
        &prefix,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo.revwalk().resolve(
        &prefix,
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Commit),
    );
    let _ = repo.revwalk().resolve(
        &peel,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo
        .revwalk()
        .resolve(&prefix, &grit_lib::revwalk::ResolveOptions::default());
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
        let _ = repo
            .revwalk()
            .resolve(&prefix, &grit_lib::revwalk::ResolveOptions::default());
        let _ = repo.revwalk().resolve(
            &prefix,
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        );
    }
}

#[test]
fn at_minus_names_previous_branch() {
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
    let name = repo.revwalk().symbolic_full_name("@{-1}");
    assert_eq!(name.as_deref(), Some("refs/heads/first"));
}

//! Additional rev-parse API coverage complementing [`rev_parse_syntax`] oracles.

use crate::support;

use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    abbreviate_ref_name, load_graft_parents, parse_index_colon_spec, parse_peel_suffix,
    reflog_date_selector_timestamp, reflog_date_selector_timestamp_at,
    revision_spec_contains_ancestry_navigation, spec_has_parent_shorthand_suffix,
    split_treeish_colon, superproject_work_tree_from_nested_git_modules, to_relative_path,
    upstream_suffix_info,
};

use support::{build_commit_dag, commit_file, git, HistoryRepo};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

#[test]
fn discovery_and_path_helpers() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    assert!(support::discover_optional(Some(wt))
        .expect("discover")
        .is_some());
    let _ = to_relative_path(wt, wt);
    assert!(superproject_work_tree_from_nested_git_modules(&wt.join(".git")).is_none());
}

#[test]
fn ref_name_and_upstream_helpers() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f", "x\n", "c");
    let repo = open_grit(wt);
    assert_eq!(abbreviate_ref_name("refs/heads/main"), "main");
    assert!(repo.revwalk().symbolic_full_name("main").is_some());
    assert!(upstream_suffix_info("main@{upstream}").is_some());
    let _ = repo.revwalk().symbolic_full_name("@{-1}");
    let _ = repo
        .revwalk()
        .resolve("@{-0}", &grit_lib::revwalk::ResolveOptions::default());
}

#[test]
fn range_and_peel_parsers() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let repo = open_grit(wt);
    assert!(repo
        .revwalk()
        .resolve(
            "main..main",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_peel(grit_lib::revwalk::Peel::Commit)
        )
        .expect("range")
        .is_some());
    assert!(revision_spec_contains_ancestry_navigation("HEAD~3"));
    assert!(spec_has_parent_shorthand_suffix("main^!"));
    let (base, peel) = parse_peel_suffix("v1^{tree}");
    assert_eq!(base, "v1");
    assert_eq!(peel, Some("tree"));
    assert_eq!(split_treeish_colon("HEAD:path"), Some(("HEAD", "path")));
    assert!(parse_index_colon_spec(":0:file").is_some());
}

#[test]
fn resolve_variants_on_linear_repo() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let repo = open_grit(wt);
    for spec in ["main", "HEAD", "HEAD~1", "main^"] {
        let _ = repo
            .revwalk()
            .resolve(
                spec,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_index_dwim(grit_lib::revwalk::IndexDwim::Off),
            )
            .expect(spec);
        let _ = repo
            .revwalk()
            .resolve(
                spec,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
            )
            .expect(spec);
        let _ = repo
            .revwalk()
            .resolve(
                spec,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                    .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
            )
            .expect(spec);
        let _ = repo
            .revwalk()
            .resolve(spec, &grit_lib::revwalk::ResolveOptions::default())
            .expect(spec);
        let _ = repo
            .revwalk()
            .resolve(
                spec,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_peel(grit_lib::revwalk::Peel::Tree),
            )
            .expect(spec);
    }
    let _ = repo.revwalk().resolve(
        "HEAD:file.txt",
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Blob),
    );
}

#[test]
fn merge_parent_navigation() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    let repo = open_grit(wt);
    let tip = repo
        .revwalk()
        .resolve(
            "main",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect("main");
    let first = repo
        .revwalk()
        .resolve("main^1", &grit_lib::revwalk::ResolveOptions::default())
        .expect("first parent");
    let second = repo
        .revwalk()
        .resolve("main^2", &grit_lib::revwalk::ResolveOptions::default())
        .expect("second parent");
    assert_ne!(first, second);
    assert_ne!(first, tip);
}

#[test]
fn abbrev_and_reflog_helpers() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f", "1\n", "one");
    commit_file(wt, "f", "2\n", "two");
    let repo = open_grit(wt);
    let head = repo
        .revwalk()
        .resolve(
            "HEAD",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect("head");
    let prefix = &head.to_hex()[..4];
    let _ = repo.revwalk().abbrev_matches(prefix).expect("all abbrev");
    let _ = repo.revwalk().abbrev_matches(prefix).expect("loose abbrev");
    let _ = repo
        .revwalk()
        .resolve(prefix, &grit_lib::revwalk::ResolveOptions::default())
        .expect("hints");
    assert!(reflog_date_selector_timestamp("1.day.ago").is_some());
    assert!(reflog_date_selector_timestamp_at("now", 1_700_000_000).is_some());
    let _ = load_graft_parents(repo.git_dir());
    let _ = repo.revwalk().upstream_symbolic_name("main@{push}");
    let _ = repo.revwalk().upstream_symbolic_name("main@{u}");
}

#[test]
fn describe_style_name_and_triple_dot() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f.txt", "1\n", "one");
    git(wt, &["tag", "-a", "v1.0", "-m", "release"]);
    commit_file(wt, "f.txt", "2\n", "two");
    git(wt, &["checkout", "-b", "side"]);
    commit_file(wt, "f.txt", "3\n", "three");
    git(wt, &["checkout", "main"]);
    let repo = open_grit(wt);
    let head = repo
        .revwalk()
        .resolve(
            "HEAD",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect("head");
    let abbrev = &head.to_hex()[..7];
    let describe = format!("v1.0-1-g{abbrev}");
    let _ = repo.revwalk().resolve(
        &describe,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo.revwalk().resolve(
        "main...side",
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Commit),
    );
    let _ = repo
        .revwalk()
        .resolve("HEAD^{tree}", &grit_lib::revwalk::ResolveOptions::default());
}

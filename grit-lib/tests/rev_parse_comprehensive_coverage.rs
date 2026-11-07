//! Broad API and spec sweeps for `rev_parse.rs` line coverage (complements oracles).

use crate::support;

use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    abbreviate_ref_name, load_graft_parents, parse_index_colon_spec, parse_peel_suffix,
    reflog_date_selector_timestamp, reflog_date_selector_timestamp_at,
    revision_spec_contains_ancestry_navigation, spec_has_parent_shorthand_suffix,
    split_double_dot_range, split_treeish_colon, split_triple_dot_range, to_relative_path,
    upstream_suffix_info,
};

use support::{build_commit_dag, commit_file, git, oracle_one, HistoryRepo, ResolveMode};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn exercise_all_resolvers(repo: &Repository, spec: &str) {
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo
        .revwalk()
        .resolve(spec, &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off),
    );
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Commit),
    );
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_peel(grit_lib::revwalk::Peel::Commit)
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off),
    );
    let _ = repo
        .revwalk()
        .resolve(spec, &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default()
            .with_purpose(grit_lib::revwalk::ResolvePurpose::CheckoutNoGuess),
    );
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Tree),
    );
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Blob),
    );
    let _ = repo.revwalk().blob_at_path(spec);
    let _ = repo.revwalk().resolve(
        spec,
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Commit),
    );
}

#[test]
fn rich_worktree_spec_sweep() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    git(wt, &["branch", "feature", "main~1"]);
    git(wt, &["branch", "side", "main~2"]);
    commit_file(wt, "tracked.txt", "hello\n", "tracked file");
    git(wt, &["tag", "-a", "release", "-m", "rel"]);
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
    let _ = repo.revwalk().peel_to_tree(head);
    let _ = repo.revwalk().peel_to_commit(head);
    let _ = repo.revwalk().peel_to_commit(head);
    let _ = repo
        .revwalk()
        .abbreviate(head, grit_lib::revwalk::AbbrevLength::AtLeast(7));
    let prefix = &head.to_hex()[..7];
    let _ = repo.revwalk().abbrev_matches(prefix);
    let _ = repo.revwalk().abbrev_matches(prefix);
    let _ = repo
        .revwalk()
        .resolve(prefix, &grit_lib::revwalk::ResolveOptions::default());
    let mut specs: Vec<String> = vec![
        "HEAD".into(),
        "main".into(),
        "feature".into(),
        "side".into(),
        "HEAD^1".into(),
        "HEAD^2".into(),
        "HEAD^!".into(),
        "HEAD^@".into(),
        "main~1".into(),
        "main~2".into(),
        "main~3".into(),
        "HEAD^{commit}".into(),
        "HEAD^{tree}".into(),
        "HEAD^{}".into(),
        "release^{commit}".into(),
        "tags/release".into(),
        "refs/tags/release".into(),
        "HEAD:tracked.txt".into(),
        ":tracked.txt".into(),
        ":0:tracked.txt".into(),
        "HEAD:".into(),
        "main:".into(),
        ":/tracked".into(),
        "main...feature".into(),
        "feature...main".into(),
        "main..feature".into(),
        "main@{0}".into(),
        "main@{1}".into(),
        "HEAD@{1979-01-01}".into(),
        "@".into(),
        "@^{commit}".into(),
        "@:tracked.txt".into(),
    ];
    for n in 0..20 {
        specs.push(format!("HEAD~{n}"));
        specs.push(format!("main^{n}"));
    }
    for spec in &specs {
        exercise_all_resolvers(&repo, spec);
    }
    exercise_all_resolvers(&repo, ":/");
    oracle_one(&repo, wt, "HEAD", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    oracle_one(&repo, wt, "HEAD:tracked.txt", ResolveMode::Verify)
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn discovery_upstream_and_parser_helpers() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f", "1\n", "one");
    let bare = tempfile::tempdir().expect("bare");
    git(bare.path(), &["init", "--bare", "-q"]);
    git(
        wt,
        &[
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("utf8"),
        ],
    );
    git(wt, &["push", "-u", "origin", "main"]);
    git(wt, &["checkout", "-b", "dev"]);
    commit_file(wt, "g", "2\n", "two");
    let repo = open_grit(wt);
    assert!(support::discover_optional(Some(wt))
        .expect("disc")
        .is_some());
    let _ = to_relative_path(&wt.join("f"), wt);
    assert_eq!(abbreviate_ref_name("refs/heads/dev"), "dev");
    assert!(repo.revwalk().symbolic_full_name("dev").is_some());
    assert!(upstream_suffix_info("dev@{upstream}").is_some());
    let _ = repo.revwalk().symbolic_full_name("@{-1}");
    let _ = repo
        .revwalk()
        .resolve("@{-0}", &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo.revwalk().upstream_symbolic_name("main@{u}");
    let _ = repo.revwalk().upstream_symbolic_name("main@{push}");
    let _ = repo.revwalk().upstream_symbolic_name("dev@{push}");
    assert!(reflog_date_selector_timestamp("1.day.ago").is_some());
    assert!(reflog_date_selector_timestamp_at("now", 1_700_000_000).is_some());
    let _ = load_graft_parents(repo.git_dir());
    assert!(revision_spec_contains_ancestry_navigation("HEAD~3"));
    assert!(spec_has_parent_shorthand_suffix("main^!"));
    let (base, peel) = parse_peel_suffix("v1^{tree}");
    assert_eq!(base, "v1");
    assert_eq!(peel, Some("tree"));
    assert_eq!(split_treeish_colon("HEAD:path"), Some(("HEAD", "path")));
    assert!(parse_index_colon_spec(":0:file").is_some());
    assert!(split_double_dot_range("a..b").is_some());
    assert!(split_triple_dot_range("a...b").is_some());
    let tip = repo
        .revwalk()
        .resolve(
            "HEAD",
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        )
        .expect("head");
    let parent = repo
        .revwalk()
        .resolve("HEAD^1", &grit_lib::revwalk::ResolveOptions::default())
        .expect("parent");
    assert_ne!(parent, tip);
    oracle_one(&repo, wt, "dev@{upstream}", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
}

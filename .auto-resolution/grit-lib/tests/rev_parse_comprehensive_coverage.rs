//! Broad API and spec sweeps for `rev_parse.rs` line coverage (complements oracles).

mod support;

use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    abbreviate_object_id, abbreviate_ref_name, ambiguous_object_hint_lines,
    commit_parents_for_navigation, discover_optional, expand_at_minus_to_branch_name,
    expand_parent_shorthand_rev_parse_lines, expand_rev_token_circ_bang, is_inside_git_dir,
    is_inside_work_tree, list_all_abbrev_matches, list_loose_abbrev_matches, load_graft_parents,
    parse_index_colon_spec, parse_peel_suffix, peel_to_commit_for_merge_base, peel_to_tree,
    reflog_date_selector_timestamp, reflog_date_selector_timestamp_at, reflog_walk_refname,
    resolve_at_minus_to_oid, resolve_index_path_entry, resolve_push_full_ref_for_branch,
    resolve_reflog_walk_log_ref, resolve_revision, resolve_revision_as_commit,
    resolve_revision_as_commit_without_index_dwim, resolve_revision_for_checkout_guess,
    resolve_revision_for_commit_tree_tree, resolve_revision_for_patch_old_blob,
    resolve_revision_for_range_end, resolve_revision_for_range_end_without_index_dwim,
    resolve_revision_for_verify, resolve_revision_without_index_dwim, resolve_treeish_blob_at_path,
    resolve_upstream_symbolic_name, revision_spec_contains_ancestry_navigation, show_prefix,
    spec_has_parent_shorthand_suffix, split_double_dot_range, split_treeish_colon,
    split_triple_dot_range, symbolic_full_name, to_relative_path, try_parse_double_dot_log_range,
    try_peel_to_commit_for_merge_base, upstream_suffix_info,
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
    let _ = resolve_revision_for_verify(repo, spec);
    let _ = resolve_revision(repo, spec);
    let _ = resolve_revision_without_index_dwim(repo, spec);
    let _ = resolve_revision_for_range_end(repo, spec);
    let _ = resolve_revision_for_range_end_without_index_dwim(repo, spec);
    let _ = resolve_revision_as_commit(repo, spec);
    let _ = resolve_revision_as_commit_without_index_dwim(repo, spec);
    let _ = resolve_revision_for_checkout_guess(repo, spec, true);
    let _ = resolve_revision_for_checkout_guess(repo, spec, false);
    let _ = resolve_revision_for_commit_tree_tree(repo, spec);
    let _ = resolve_revision_for_patch_old_blob(repo, spec);
    let _ = resolve_treeish_blob_at_path(repo, spec);
    let _ = resolve_index_path_entry(repo, spec);
    let _ = try_parse_double_dot_log_range(repo, spec);
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
    let head = resolve_revision_for_verify(&repo, "HEAD").expect("head");
    let _ = peel_to_tree(&repo, head);
    let _ = peel_to_commit_for_merge_base(&repo, head);
    let _ = try_peel_to_commit_for_merge_base(&repo, head);
    let _ = abbreviate_object_id(&repo, head, 7);
    let prefix = &head.to_hex()[..7];
    let _ = list_all_abbrev_matches(&repo, prefix);
    let _ = list_loose_abbrev_matches(&repo, prefix);
    let _ = ambiguous_object_hint_lines(&repo, prefix, None);
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
    assert!(discover_optional(Some(wt)).expect("disc").is_some());
    assert!(is_inside_work_tree(&repo, wt));
    assert!(!is_inside_git_dir(&repo, wt));
    let _ = show_prefix(&repo, wt);
    let _ = to_relative_path(&wt.join("f"), wt);
    assert_eq!(abbreviate_ref_name("refs/heads/dev"), "dev");
    assert!(symbolic_full_name(&repo, "dev").is_some());
    assert!(upstream_suffix_info("dev@{upstream}").is_some());
    let _ = expand_at_minus_to_branch_name(&repo, "@{-1}");
    let _ = resolve_at_minus_to_oid(&repo, "@{-0}");
    let _ = resolve_upstream_symbolic_name(&repo, "main@{u}");
    let _ = resolve_push_full_ref_for_branch(&repo, "main");
    let _ = resolve_push_full_ref_for_branch(&repo, "dev");
    let _ = reflog_walk_refname(&repo, "HEAD@{0}");
    let _ = resolve_reflog_walk_log_ref(&repo, "HEAD");
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
    let lines = expand_parent_shorthand_rev_parse_lines(&repo, "HEAD^!", false, None)
        .expect("expand")
        .expect("lines");
    assert!(!lines.is_empty());
    let circ = expand_rev_token_circ_bang(&repo, "HEAD").expect("circ");
    assert!(!circ.is_empty());
    let tip = resolve_revision_for_verify(&repo, "HEAD").expect("head");
    let parents = commit_parents_for_navigation(&repo, tip).expect("parents");
    assert!(!parents.is_empty());
    oracle_one(&repo, wt, "dev@{upstream}", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
}

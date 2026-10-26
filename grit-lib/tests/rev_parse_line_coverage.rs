//! Additional rev-parse API coverage complementing [`rev_parse_syntax`] oracles.

use crate::support;

use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    abbreviate_ref_name, ambiguous_object_hint_lines, commit_parents_for_navigation,
    discover_optional, expand_at_minus_to_branch_name, expand_parent_shorthand_rev_parse_lines,
    expand_rev_token_circ_bang, is_inside_git_dir, is_inside_work_tree, list_all_abbrev_matches,
    list_loose_abbrev_matches, load_graft_parents, parse_index_colon_spec, parse_peel_suffix,
    reflog_date_selector_timestamp, reflog_date_selector_timestamp_at, reflog_walk_refname,
    resolve_at_minus_to_oid, resolve_index_path_entry, resolve_push_full_ref_for_branch,
    resolve_reflog_walk_log_ref, resolve_revision_for_checkout_guess,
    resolve_revision_for_commit_tree_tree, resolve_revision_for_patch_old_blob,
    resolve_revision_for_range_end, resolve_revision_for_range_end_without_index_dwim,
    resolve_revision_without_index_dwim, resolve_upstream_symbolic_name,
    revision_spec_contains_ancestry_navigation, show_prefix, spec_has_parent_shorthand_suffix,
    split_treeish_colon, superproject_work_tree_from_nested_git_modules, symbolic_full_name,
    to_relative_path, try_parse_double_dot_log_range, upstream_suffix_info,
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
    assert!(discover_optional(Some(wt)).expect("discover").is_some());
    assert!(is_inside_work_tree(&repo, wt));
    assert!(!is_inside_git_dir(&repo, wt));
    let _ = show_prefix(&repo, wt);
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
    assert!(symbolic_full_name(&repo, "main").is_some());
    assert!(upstream_suffix_info("main@{upstream}").is_some());
    let _ = expand_at_minus_to_branch_name(&repo, "@{-1}");
    let _ = resolve_at_minus_to_oid(&repo, "@{-0}");
}

#[test]
fn range_and_peel_parsers() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(wt, &[("b", "a")], &[("b", "refs/heads/main")]);
    let repo = open_grit(wt);
    assert!(try_parse_double_dot_log_range(&repo, "main..main")
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
        let _ = resolve_revision_without_index_dwim(&repo, spec).expect(spec);
        let _ = resolve_revision_for_range_end(&repo, spec).expect(spec);
        let _ = resolve_revision_for_range_end_without_index_dwim(&repo, spec).expect(spec);
        let _ = resolve_revision_for_checkout_guess(&repo, spec, true).expect(spec);
        let _ = resolve_revision_for_commit_tree_tree(&repo, spec).expect(spec);
    }
    let _ = resolve_revision_for_patch_old_blob(&repo, "HEAD:file.txt");
    let _ = resolve_index_path_entry(&repo, ":file.txt");
}

#[test]
fn parent_shorthand_and_circ_bang_expansion() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    let repo = open_grit(wt);
    let lines = expand_parent_shorthand_rev_parse_lines(&repo, "main^@", false, Some(7))
        .expect("expand ^@")
        .expect("lines");
    assert!(!lines.is_empty());
    let circ = expand_rev_token_circ_bang(&repo, "main").expect("circ");
    assert!(!circ.is_empty());
    let tip =
        grit_lib::revwalk::rev_parse::resolve_revision_for_verify(&repo, "main").expect("main");
    let parents = commit_parents_for_navigation(&repo, tip).expect("parents");
    assert!(!parents.is_empty());
}

#[test]
fn abbrev_and_reflog_helpers() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "f", "1\n", "one");
    commit_file(wt, "f", "2\n", "two");
    let repo = open_grit(wt);
    let head =
        grit_lib::revwalk::rev_parse::resolve_revision_for_verify(&repo, "HEAD").expect("head");
    let prefix = &head.to_hex()[..4];
    let _ = list_all_abbrev_matches(&repo, prefix).expect("all abbrev");
    let _ = list_loose_abbrev_matches(&repo, prefix).expect("loose abbrev");
    let _ = ambiguous_object_hint_lines(&repo, prefix, None).expect("hints");
    let _ = reflog_walk_refname(&repo, "HEAD@{0}");
    assert!(reflog_date_selector_timestamp("1.day.ago").is_some());
    assert!(reflog_date_selector_timestamp_at("now", 1_700_000_000).is_some());
    let _ = load_graft_parents(repo.git_dir());
    let _ = resolve_reflog_walk_log_ref(&repo, "HEAD");
    let _ = resolve_reflog_walk_log_ref(&repo, "main");
    let _ = resolve_reflog_walk_log_ref(&repo, "@{-1}");
    let _ = resolve_push_full_ref_for_branch(&repo, "main");
    let _ = resolve_upstream_symbolic_name(&repo, "main@{u}");
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
    let head =
        grit_lib::revwalk::rev_parse::resolve_revision_for_verify(&repo, "HEAD").expect("head");
    let abbrev = &head.to_hex()[..7];
    let describe = format!("v1.0-1-g{abbrev}");
    let _ = grit_lib::revwalk::rev_parse::resolve_revision_for_verify(&repo, &describe);
    let _ = grit_lib::revwalk::rev_parse::resolve_revision_as_commit(&repo, "main...side");
    let _ = grit_lib::revwalk::rev_parse::resolve_revision(&repo, "HEAD^{tree}");
}

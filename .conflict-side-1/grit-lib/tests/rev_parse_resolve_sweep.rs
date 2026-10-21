//! Wide resolver sweep to raise `rev_parse.rs` line coverage (complements oracles).

mod support;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    abbreviate_object_id, ambiguous_object_hint_lines, expand_at_minus_to_branch_name,
    expand_parent_shorthand_rev_parse_lines, expand_rev_token_circ_bang, list_all_abbrev_matches,
    list_loose_abbrev_matches, load_graft_parents, parse_index_colon_spec, parse_peel_suffix,
    peel_to_commit_for_merge_base, peel_to_tree, reflog_date_selector_timestamp,
    reflog_date_selector_timestamp_at, reflog_walk_refname, resolve_at_minus_to_oid,
    resolve_index_path_entry, resolve_push_full_ref_for_branch, resolve_reflog_walk_log_ref,
    resolve_revision, resolve_revision_as_commit, resolve_revision_as_commit_without_index_dwim,
    resolve_revision_for_checkout_guess, resolve_revision_for_commit_tree_tree,
    resolve_revision_for_patch_old_blob, resolve_revision_for_range_end,
    resolve_revision_for_range_end_without_index_dwim, resolve_revision_for_verify,
    resolve_revision_without_index_dwim, resolve_treeish_blob_at_path,
    resolve_upstream_symbolic_name, split_double_dot_range, split_treeish_colon,
    split_triple_dot_range, symbolic_full_name, try_peel_to_commit_for_merge_base,
};

use support::{build_commit_dag, commit_file, git, HistoryRepo};

fn open_grit(wt: &std::path::Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

fn exercise_all(repo: &Repository, spec: &str) {
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
}

#[test]
fn deep_history_resolver_sweep() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    let mut edge_labels: Vec<(String, String)> = Vec::new();
    for i in 1..=80 {
        let child = format!("c{i}");
        let parent = if i == 1 {
            "root".to_string()
        } else {
            format!("c{}", i - 1)
        };
        edge_labels.push((child, parent));
    }
    let edges: Vec<(&str, &str)> = edge_labels
        .iter()
        .map(|(c, p)| (c.as_str(), p.as_str()))
        .collect();
    build_commit_dag(wt, &edges, &[("c80", "refs/heads/main")]);
    git(wt, &["branch", "side", "main~20"]);
    git(wt, &["branch", "feature", "main~40"]);
    commit_file(wt, "tracked.txt", "body\n", "tracked");
    git(wt, &["tag", "-a", "rel", "-m", "release"]);
    git(wt, &["config", "push.default", "simple"]);
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
    git(wt, &["fetch", "origin"]);

    let repo = open_grit(wt);
    let head = resolve_revision_for_verify(&repo, "HEAD").expect("head");
    let _ = peel_to_tree(&repo, head);
    let _ = peel_to_commit_for_merge_base(&repo, head);
    let _ = try_peel_to_commit_for_merge_base(&repo, head);
    let prefix = &head.to_hex()[..7];
    let _ = list_all_abbrev_matches(&repo, prefix);
    let _ = list_loose_abbrev_matches(&repo, prefix);
    let _ = ambiguous_object_hint_lines(&repo, prefix, Some("commit"));
    let _ = ambiguous_object_hint_lines(&repo, prefix, None);
    let _ = abbreviate_object_id(&repo, head, 7);
    let _ = symbolic_full_name(&repo, "main");
    let _ = resolve_upstream_symbolic_name(&repo, "main@{upstream}");
    let _ = resolve_push_full_ref_for_branch(&repo, "main");
    let _ = expand_at_minus_to_branch_name(&repo, "@{-1}");
    let _ = resolve_at_minus_to_oid(&repo, "@{-1}");
    let _ = reflog_walk_refname(&repo, "HEAD@{0}");
    let _ = resolve_reflog_walk_log_ref(&repo, "HEAD");
    let _ = resolve_reflog_walk_log_ref(&repo, "main");
    let _ = expand_rev_token_circ_bang(&repo, "main^!");
    let _ = expand_parent_shorthand_rev_parse_lines(&repo, "main^@", false, Some(7));
    let _ = load_graft_parents(repo.git_dir());
    assert!(reflog_date_selector_timestamp("1.day.ago").is_some());
    let _ = reflog_date_selector_timestamp_at("2.weeks.ago", 1_700_000_000);
    let (base, peel) = parse_peel_suffix("HEAD^{tree}");
    assert_eq!(base, "HEAD");
    assert_eq!(peel, Some("tree"));
    assert!(parse_index_colon_spec(":0:tracked.txt").is_some());
    assert!(split_treeish_colon("HEAD:tracked.txt").is_some());
    assert!(split_double_dot_range("main..feature").is_some());
    assert!(split_triple_dot_range("main...feature").is_some());

    if let Ok(prefixes) = list_all_abbrev_matches(&repo, &head.to_hex()[..8]) {
        if prefixes.len() > 1 {
            let p = &prefixes[0].to_hex()[..8];
            let _ = resolve_revision_as_commit(&repo, p);
            let _ = resolve_revision_for_verify(&repo, p);
            let _ = ambiguous_object_hint_lines(&repo, p, Some("commit"));
        }
    }

    let mut specs: Vec<String> = (0..80)
        .flat_map(|n| {
            [
                format!("HEAD~{n}"),
                format!("main~{n}"),
                format!("HEAD~{n}^{{tree}}"),
                format!("HEAD~{n}^{{commit}}"),
            ]
        })
        .chain([
            "main".into(),
            "side".into(),
            "feature".into(),
            "rel".into(),
            "tags/rel".into(),
            "HEAD^!".into(),
            "HEAD^@".into(),
            "main^-2".into(),
            "main...feature".into(),
            "feature...main".into(),
            "main..side".into(),
            ":/history harness c80".into(),
            ":0:/etc/passwd".into(),
            ":2:/etc/passwd".into(),
            ":/!beta".into(),
            "HEAD:tracked.txt".into(),
            ":tracked.txt".into(),
            ":0:tracked.txt".into(),
            "remotes/origin/main".into(),
            "origin/main".into(),
            "main@{0}".into(),
            "main@{1}".into(),
            "HEAD@{1979-01-01}".into(),
            "@".into(),
            "FETCH_HEAD".into(),
        ])
        .collect();
    for n in 1..=3 {
        specs.push(format!("main^^{n}"));
    }
    for spec in &specs {
        exercise_all(&repo, spec);
    }
}

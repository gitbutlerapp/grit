//! Scenario-based rev-parse coverage (special syntax paths in `resolve_revision_impl`).

mod support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    expand_parent_shorthand_rev_parse_lines, resolve_push_full_ref_for_branch, resolve_revision,
    resolve_revision_as_commit, resolve_revision_as_commit_without_index_dwim,
    resolve_revision_for_patch_old_blob, resolve_revision_for_verify, resolve_treeish_blob_at_path,
    resolve_upstream_symbolic_name,
};

use support::{
    build_commit_dag, commit_file, git, oracle_one, oracle_table, HistoryRepo, ResolveMode,
};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover")
}

#[test]
fn triple_dot_tags_auto_merge_and_refs() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    git(wt, &["branch", "feature", "main~1"]);
    commit_file(wt, "tagged.txt", "t\n", "tagged");
    git(wt, &["tag", "v1"]);
    let tree = git(wt, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    fs::write(wt.join(".git/AUTO_MERGE"), format!("{tree}\n")).expect("AUTO_MERGE");
    let repo = open_grit(wt);
    for spec in [
        "main...feature",
        "feature...main",
        "main...",
        "...feature",
        "tags/v1",
        "refs/tags/v1",
        "refs/heads/main",
        "AUTO_MERGE",
        ":/history harness c",
    ] {
        let _ = resolve_revision_for_verify(&repo, spec);
        let _ = resolve_revision(&repo, spec);
    }
    oracle_one(&repo, wt, "tags/v1", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn parent_shorthand_expansion_symbolic_and_abbrev() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    let repo = open_grit(wt);
    let lines = expand_parent_shorthand_rev_parse_lines(&repo, "main^!", false, None)
        .expect("expand")
        .expect("lines");
    assert!(!lines.is_empty());
    let sym = expand_parent_shorthand_rev_parse_lines(&repo, "main^!", true, None)
        .expect("sym")
        .expect("sym lines");
    assert!(!sym.is_empty());
    let abbrev = expand_parent_shorthand_rev_parse_lines(&repo, "main^!", false, Some(7))
        .expect("abbrev")
        .expect("abbrev lines");
    assert!(!abbrev.is_empty());
    let at = expand_parent_shorthand_rev_parse_lines(&repo, "main^@", false, None)
        .expect("at")
        .expect("at lines");
    assert!(!at.is_empty());
    let single = HistoryRepo::init();
    let swt = single.worktree();
    commit_file(swt, "f", "1\n", "only");
    let srepo = open_grit(swt);
    let bang_only = expand_parent_shorthand_rev_parse_lines(&srepo, "HEAD^!", false, None)
        .expect("single bang")
        .expect("single");
    assert_eq!(bang_only.len(), 1);
}

#[test]
fn resolve_variant_matrix_on_rich_repo() {
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
    git(wt, &["branch", "side", "main~10"]);
    commit_file(wt, "side.txt", "s\n", "side-tip");
    let repo = open_grit(wt);
    let specs: Vec<String> = (0..80)
        .map(|n| format!("HEAD~{n}"))
        .chain([
            "main^{commit}".to_string(),
            "main^{tree}".to_string(),
            "main^{/history harness c80}".to_string(),
            "side~0".to_string(),
        ])
        .collect();
    let spec_refs: Vec<&str> = specs.iter().map(String::as_str).collect();
    oracle_table(&repo, wt, ResolveMode::Verify, &spec_refs);
    let _ = resolve_revision_for_verify(&repo, "main...side");
    for n in 1..=5 {
        let spec = format!("main^^{n}");
        let _ = resolve_revision_as_commit(&repo, &spec);
        let _ = resolve_revision_as_commit_without_index_dwim(&repo, &spec);
    }
    let _ = resolve_treeish_blob_at_path(&repo, "HEAD:side.txt");
    let _ = resolve_revision_for_patch_old_blob(&repo, "HEAD:side.txt");
}

#[test]
fn upstream_and_push_resolution_smoke() {
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
    let repo = open_grit(wt);
    let _ = resolve_upstream_symbolic_name(&repo, "main@{u}");
    let _ = resolve_push_full_ref_for_branch(&repo, "main");
    let tree = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, b"")
        .expect("tree");
    let _ = resolve_revision_for_verify(&repo, &tree.to_hex()[..7]);
}

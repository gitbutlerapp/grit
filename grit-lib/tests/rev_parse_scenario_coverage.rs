//! Scenario-based rev-parse coverage (special syntax paths in `resolve_revision_impl`).

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;

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
        let _ = repo.revwalk().resolve(
            spec,
            &grit_lib::revwalk::ResolveOptions::default()
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
        );
        let _ = repo
            .revwalk()
            .resolve(spec, &grit_lib::revwalk::ResolveOptions::default());
    }
    oracle_one(&repo, wt, "tags/v1", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
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
    let _ = repo.revwalk().resolve(
        "main...side",
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
    for n in 1..=5 {
        let spec = format!("main^^{n}");
        let _ = repo.revwalk().resolve(
            &spec,
            &grit_lib::revwalk::ResolveOptions::default()
                .with_peel(grit_lib::revwalk::Peel::Commit),
        );
        let _ = repo.revwalk().resolve(
            &spec,
            &grit_lib::revwalk::ResolveOptions::default()
                .with_peel(grit_lib::revwalk::Peel::Commit)
                .with_index_dwim(grit_lib::revwalk::IndexDwim::Off),
        );
    }
    let _ = repo.revwalk().blob_at_path("HEAD:side.txt");
    let _ = repo.revwalk().resolve(
        "HEAD:side.txt",
        &grit_lib::revwalk::ResolveOptions::default().with_peel(grit_lib::revwalk::Peel::Blob),
    );
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
    let _ = repo.revwalk().upstream_symbolic_name("main@{u}");
    let _ = repo.revwalk().upstream_symbolic_name("main@{push}");
    let tree = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, b"")
        .expect("tree");
    let _ = repo.revwalk().resolve(
        &tree.to_hex()[..7],
        &grit_lib::revwalk::ResolveOptions::default()
            .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
            .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
    );
}

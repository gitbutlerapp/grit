//! Wide resolver sweep to raise `rev_parse.rs` line coverage (complements oracles).

use crate::support;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    load_graft_parents, parse_index_colon_spec, parse_peel_suffix, reflog_date_selector_timestamp,
    reflog_date_selector_timestamp_at, split_double_dot_range, split_treeish_colon,
    split_triple_dot_range,
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
    let prefix = &head.to_hex()[..7];
    let _ = repo.revwalk().abbrev_matches(prefix);
    let _ = repo.revwalk().abbrev_matches(prefix);
    let _ = repo
        .revwalk()
        .resolve(prefix, &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo
        .revwalk()
        .resolve(prefix, &grit_lib::revwalk::ResolveOptions::default());
    let _ = repo
        .revwalk()
        .abbreviate(head, grit_lib::revwalk::AbbrevLength::AtLeast(7));
    let _ = repo.revwalk().symbolic_full_name("main");
    let _ = repo.revwalk().upstream_symbolic_name("main@{upstream}");
    let _ = repo.revwalk().upstream_symbolic_name("main@{push}");
    let _ = repo.revwalk().symbolic_full_name("@{-1}");
    let _ = repo
        .revwalk()
        .resolve("@{-1}", &grit_lib::revwalk::ResolveOptions::default());
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

    if let Ok(prefixes) = repo.revwalk().abbrev_matches(&head.to_hex()[..8]) {
        if prefixes.len() > 1 {
            let p = &prefixes[0].to_hex()[..8];
            let _ = repo.revwalk().resolve(
                p,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_peel(grit_lib::revwalk::Peel::Commit),
            );
            let _ = repo.revwalk().resolve(
                p,
                &grit_lib::revwalk::ResolveOptions::default()
                    .with_index_dwim(grit_lib::revwalk::IndexDwim::Off)
                    .with_purpose(grit_lib::revwalk::ResolvePurpose::RangeEnd),
            );
            let _ = repo
                .revwalk()
                .resolve(p, &grit_lib::revwalk::ResolveOptions::default());
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

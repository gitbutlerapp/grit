//! Exercise `porcelain::log` decoration collection for total coverage.

mod support;

use grit_lib::objects::ObjectId;
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::log::{
    collect_decorations, collect_decorations_for_commits, collect_decorations_inner,
    current_branch_decoration_index, normalize_glob_ref, peel_to_commit_hex, DecorationFilter,
    DecorationKind,
};
use grit_lib::state::HeadState;
use std::fs;
use support::{commit_file, git, open_grit_repo, HistoryRepo};

#[test]
fn log_decorations_cover_all_ref_namespaces() -> grit_lib::error::Result<()> {
    assert_eq!(normalize_glob_ref("heads/*"), "refs/heads/*");
    assert_eq!(normalize_glob_ref("refs/tags/v"), "refs/tags/v");
    assert_eq!(normalize_glob_ref("HEAD"), "HEAD");

    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "a.txt", "a\n", "init");
    git(wt, &["tag", "v1"]);
    git(wt, &["branch", "topic"]);
    git(
        wt,
        &["remote", "add", "origin", wt.to_str().expect("utf8 path")],
    );
    git(wt, &["fetch", "origin"]);
    git(wt, &["update-ref", "refs/hidden/extra", "HEAD"]);
    git(wt, &["stash", "push", "-m", "stash me"]);

    let repo = open_grit_repo(wt);
    let map_full = collect_decorations(&repo, true)?;
    let map_short = collect_decorations(&repo, false)?;
    assert!(!map_full.is_empty());
    assert!(!map_short.is_empty());
    assert_targeted_matches_full(&repo, wt);

    let filter = DecorationFilter {
        include: vec!["refs/heads/*".into()],
        exclude: vec!["refs/heads/topic".into()],
        exclude_config: vec!["refs/remotes/origin/HEAD".into()],
    };
    assert!(filter.matches("refs/heads/main"));
    assert!(!filter.matches("refs/heads/topic"));

    let cleared = collect_decorations_inner(&repo, false, true, &DecorationFilter::default())?;
    assert!(cleared
        .values()
        .flatten()
        .any(|it| it.kind == DecorationKind::Other));

    let filtered = collect_decorations_inner(&repo, true, false, &filter)?;
    assert!(!filtered.is_empty());

    fs::write(
        repo.git_dir().join("config"),
        "[grit]\n\tsubmoduleUpdateRemoteDecorations = true\n",
    )
    .map_err(grit_lib::error::Error::Io)?;
    repo.reload_config()?;
    let _ = collect_decorations(&repo, false)?;

    let tip_hex = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
    let peeled = peel_to_commit_hex(repo.odb(), &tip_hex).expect("peel tip");
    assert_eq!(peeled.len(), 40);

    let tag_oid = git(wt, &["rev-parse", "v1"]).trim().to_string();
    let _ = peel_to_commit_hex(repo.odb(), &tag_oid);

    let blob = repo.odb().write(ObjectKind::Blob, b"x\n")?;
    assert!(peel_to_commit_hex(repo.odb(), &blob.to_hex()).is_none());

    let head = grit_lib::state::resolve_head(repo.git_dir())?;
    if let Some(items) = map_short.get(&tip_hex) {
        let _ = current_branch_decoration_index(items, &head);
    }
    if let HeadState::Branch { refname, .. } = &head {
        let items = vec![
            grit_lib::porcelain::log::DecorationItem {
                refname: Some("HEAD".into()),
                display: "HEAD".into(),
                kind: DecorationKind::Head,
            },
            grit_lib::porcelain::log::DecorationItem {
                refname: Some(refname.clone()),
                display: refname.clone(),
                kind: DecorationKind::Branch,
            },
        ];
        assert_eq!(current_branch_decoration_index(&items, &head), Some(1));
    }

    Ok(())
}

/// Sorted `(commit hex, decorations)` pairs, for comparing two decoration maps.
fn decoration_rows(
    map: &grit_lib::porcelain::log::DecorationMap,
) -> Vec<(String, Vec<(Option<String>, String)>)> {
    let mut rows: Vec<_> = map
        .iter()
        .map(|(hex, items)| {
            let items = items
                .iter()
                .map(|it| (it.refname.clone(), it.display.clone()))
                .collect();
            (hex.clone(), items)
        })
        .collect();
    rows.sort();
    rows
}

/// `collect_decorations_for_commits` must equal the full map restricted to its commits.
fn assert_targeted_matches_full(repo: &grit_lib::repo::Repository, wt: &std::path::Path) {
    let all: Vec<ObjectId> = git(wt, &["rev-list", "--all"])
        .lines()
        .map(|l| l.parse().expect("oid"))
        .collect();
    for full in [false, true] {
        let whole = collect_decorations(repo, full).expect("full map");
        for chunk in all.chunks(1).chain(std::iter::once(&all[..])) {
            let targeted = collect_decorations_for_commits(repo, full, chunk).expect("targeted");
            let mut expected = whole.clone();
            expected.retain(|hex, _| chunk.iter().any(|oid| &oid.to_hex() == hex));
            assert_eq!(
                decoration_rows(&targeted),
                decoration_rows(&expected),
                "full={full} commits={chunk:?}"
            );
        }
    }
}

#[test]
fn packed_ref_peel_lines_decorate_like_loose_tags() -> grit_lib::error::Result<()> {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "a.txt", "a\n", "first");
    let first = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
    git(wt, &["tag", "-a", "-m", "annotated", "v1"]);
    git(wt, &["tag", "-a", "-m", "nested", "v1-nested", "v1"]);
    git(wt, &["tag", "light"]);
    let tree = git(wt, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    git(wt, &["tag", "-a", "-m", "tree tag", "tree-tag", &tree]);
    commit_file(wt, "b.txt", "b\n", "second");
    git(wt, &["tag", "-a", "-m", "moves later", "moving", &first]);
    git(wt, &["branch", "topic", &first]);

    let repo = open_grit_repo(wt);
    let loose = decoration_rows(&collect_decorations(&repo, false)?);
    let loose_full = decoration_rows(&collect_decorations(&repo, true)?);
    assert!(
        loose
            .iter()
            .any(|(hex, items)| hex == &first && items.iter().any(|(_, d)| d == "v1-nested")),
        "nested annotated tag decorates its commit: {loose:?}"
    );
    assert_targeted_matches_full(&repo, wt);

    git(wt, &["pack-refs", "--all"]);
    let packed_refs = fs::read_to_string(repo.git_dir().join("packed-refs")).expect("packed-refs");
    assert!(packed_refs.contains("fully-peeled"), "{packed_refs}");
    assert!(packed_refs.lines().any(|l| l.starts_with('^')));
    let repo = open_grit_repo(wt);
    assert_eq!(decoration_rows(&collect_decorations(&repo, false)?), loose);
    assert_eq!(
        decoration_rows(&collect_decorations(&repo, true)?),
        loose_full
    );

    // A loose ref now overrides the packed `moving` tag (and its stale peel line).
    git(wt, &["tag", "-f", "-a", "-m", "moved", "moving", "HEAD"]);
    let repo = open_grit_repo(wt);
    let moved = decoration_rows(&collect_decorations(&repo, false)?);
    let head = git(wt, &["rev-parse", "HEAD"]).trim().to_string();
    let decorated_with_moving = |rows: &[(String, Vec<(Option<String>, String)>)]| {
        rows.iter()
            .filter(|(_, items)| items.iter().any(|(_, d)| d == "moving"))
            .map(|(hex, _)| hex.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(decorated_with_moving(&moved), vec![head]);
    assert_targeted_matches_full(&repo, wt);
    assert_eq!(decorated_with_moving(&loose), vec![first]);

    Ok(())
}

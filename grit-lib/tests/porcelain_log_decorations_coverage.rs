//! Exercise `porcelain::log` decoration collection for total coverage.

mod support;

use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::log::{
    collect_decorations, collect_decorations_inner, current_branch_decoration_index,
    normalize_glob_ref, peel_to_commit_hex, DecorationFilter, DecorationKind,
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

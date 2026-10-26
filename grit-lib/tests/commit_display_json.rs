//! `load_commits_with_decorations` loads parents and ref labels in one pass.

use crate::support;

use grit_lib::change_id::{resolved_change_id, ChangeId};
use grit_lib::objects::{parse_commit, serialize_commit, ObjectId, OidHashMap};
use grit_lib::porcelain::log::{
    collect_decorations, load_commits_with_decorations, load_commits_with_decorations_reusing,
    DecorationKind,
};
use support::{commit_file, open_grit_repo, HistoryRepo};

#[test]
fn load_commits_attaches_branch_and_parents() -> grit_lib::error::Result<()> {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "a.txt", "a\n", "root");
    commit_file(wt, "a.txt", "b\n", "tip");
    let repo = open_grit_repo(wt);
    let head: ObjectId = repo.refs().resolve("HEAD")?;

    let decorations = collect_decorations(&repo, false)?;
    let loaded = load_commits_with_decorations(&repo, std::slice::from_ref(&head), &decorations)?;
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].oid, head);
    assert_eq!(loaded[0].parents.len(), 1);
    assert!(loaded[0]
        .refs
        .iter()
        .any(|r| r.kind == DecorationKind::Branch));
    Ok(())
}

#[test]
fn loaded_commit_reports_stored_and_derived_change_id() -> grit_lib::error::Result<()> {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "a.txt", "a\n", "root");
    let repo = open_grit_repo(wt);
    let head: ObjectId = repo.refs().resolve("HEAD")?;
    let obj = repo.odb().read(&head)?;
    let data = parse_commit(&obj.data)?;
    let derived = load_commits_with_decorations(
        &repo,
        std::slice::from_ref(&head),
        &collect_decorations(&repo, false)?,
    )?;
    assert!(!derived[0].change_id_stored);
    assert_eq!(
        derived[0].change_id.as_str(),
        ChangeId::derived_from_commit_id(&head).as_str()
    );

    let stored = ChangeId::from_bytes(&[0x11; 16]);
    let mut with_header = data.clone();
    with_header.extra_headers = format!("change-id {}\n", stored.as_str()).into_bytes();
    let raw = serialize_commit(&with_header);
    let parsed = parse_commit(&raw)?;
    let resolved = resolved_change_id(&head, &parsed);
    assert!(resolved.stored);
    assert_eq!(resolved.id.as_str(), stored.as_str());
    Ok(())
}

#[test]
fn reusing_parsed_commits_matches_reading_them() -> grit_lib::error::Result<()> {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    commit_file(wt, "a.txt", "a\n", "root");
    commit_file(wt, "a.txt", "b\n", "middle");
    commit_file(wt, "a.txt", "c\n", "tip");
    let repo = open_grit_repo(wt);
    let tip: ObjectId = repo.refs().resolve("HEAD")?;
    let middle = parse_commit(&repo.odb().read(&tip)?.data)?.parents[0];
    let root = parse_commit(&repo.odb().read(&middle)?.data)?.parents[0];
    let oids = [tip, middle, root];
    let decorations = collect_decorations(&repo, false)?;
    let expected = load_commits_with_decorations(&repo, &oids, &decorations)?;

    // Only some commits are pre-parsed; an unrelated entry must be left alone.
    let mut parsed = OidHashMap::default();
    parsed.insert(tip, parse_commit(&repo.odb().read(&tip)?.data)?);
    parsed.insert(root, parse_commit(&repo.odb().read(&root)?.data)?);
    let unrelated: ObjectId = "0123456789abcdef0123456789abcdef01234567"
        .parse()
        .expect("oid");
    parsed.insert(unrelated, parse_commit(&repo.odb().read(&middle)?.data)?);

    let reused = load_commits_with_decorations_reusing(&repo, &oids, &decorations, &mut parsed)?;
    assert_eq!(reused, expected);
    assert_eq!(parsed.len(), 1, "listed commits are moved out of the map");
    assert!(parsed.contains_key(&unrelated));

    let mut empty = OidHashMap::default();
    let read_all = load_commits_with_decorations_reusing(&repo, &oids, &decorations, &mut empty)?;
    assert_eq!(read_all, expected);
    Ok(())
}

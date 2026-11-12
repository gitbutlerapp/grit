//! Unicode normalization checkout collisions: warn and auto-stage safety for loser paths.

use grit_lib::error::Error;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{entry_from_stat, Index, MODE_REGULAR};
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::commit::{CommitRequest, CommitScope};
use grit_lib::porcelain::status::StatusOptions;
use grit_lib::porcelain::CommitError;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use std::fs;
use std::path::Path;

fn enable_precompose(repo: &Repository, root: &Path) {
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tprecomposeunicode = true\n",
    )
    .expect("config");
    repo.reload_config().expect("reload");
}

/// Whether this volume resolves NFC and NFD spellings to one file (APFS, HFS+).
///
/// There checkout leaves a single dentry for the collision group; elsewhere both
/// spellings are written as separate files.
fn aliases_nfc_and_nfd(dir: &Path) -> bool {
    grit_lib::test_support::fs_probe::is_normalization_insensitive(dir)
}

fn build_nfc_nfd_collision_tree(repo: &Repository) -> grit_lib::objects::ObjectId {
    let wt = repo.work_tree().expect("worktree");
    let nfc = "caf\u{00e9}.txt";
    let nfd = "cafe\u{0301}.txt";
    let oid_nfc = repo.odb().write(ObjectKind::Blob, b"NFC\n").expect("nfc");
    let oid_nfd = repo.odb().write(ObjectKind::Blob, b"NFD\n").expect("nfd");
    fs::write(wt.join(nfc), b"NFC\n").expect("nfc file for index stat");
    fs::write(wt.join(nfd), b"NFC\n").expect("nfd file for index stat");
    let mut index = Index::new();
    index.add_or_replace(
        entry_from_stat(&wt.join(nfc), nfc.as_bytes(), oid_nfc, MODE_REGULAR).expect("nfc entry"),
    );
    index.add_or_replace(
        entry_from_stat(&wt.join(nfd), nfd.as_bytes(), oid_nfd, MODE_REGULAR).expect("nfd entry"),
    );
    fs::write(wt.join("ok.txt"), b"ok\n").expect("ok file");
    let oid_ok = repo
        .odb()
        .write(ObjectKind::Blob, b"ok\n")
        .expect("ok blob");
    index.add_or_replace(
        entry_from_stat(&wt.join("ok.txt"), b"ok.txt", oid_ok, MODE_REGULAR).expect("ok"),
    );
    index.sort();
    write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent()).expect("tree")
}

#[test]
fn populated_checkout_reports_nfc_collisions_and_auto_stage_skips_losers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("grit-wt");
    fs::create_dir(&dest).expect("mkdir");
    let (repo, _) = Repository::init(
        &dest,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    enable_precompose(&repo, &dest);

    let nfc = "caf\u{00e9}.txt";
    let nfd = "cafe\u{0301}.txt";
    let tree = build_nfc_nfd_collision_tree(&repo);
    let _ = fs::remove_file(dest.join(nfc));
    let _ = fs::remove_file(dest.join(nfd));
    // Single on-disk spelling (as on macOS): content matches NFC row.
    fs::write(dest.join(nfd), b"NFC\n").expect("one shared file");

    let checkout = repo
        .checkout()
        .checkout(None, &tree, &mut NullProgress, &CancelToken::never())
        .expect("checkout");

    assert!(
        checkout.path_collisions.iter().any(|p| p == nfc),
        "expected NFC path in collisions: {:?}",
        checkout.path_collisions
    );
    assert!(
        checkout.path_collisions.iter().any(|p| p == nfd),
        "expected NFD path in collisions: {:?}",
        checkout.path_collisions
    );

    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let nfc_oid = index.get(nfc.as_bytes(), 0).expect(nfc).oid;
    let nfd_oid = index.get(nfd.as_bytes(), 0).expect(nfd).oid;
    assert_ne!(nfc_oid, nfd_oid);

    fs::write(dest.join("unrelated.txt"), "edit\n").expect("unrelated");
    repo.index()
        .stage_tracked_modifications(&dest)
        .expect("stage tracked");

    let id = "Tester <t@example.com> 1700000000 +0000".to_owned();
    let req = {
        let mut req = CommitRequest::new("unrelated".to_owned());
        req.author = Some(id.parse().expect("sig"));
        req.committer = Some(id.parse().expect("sig"));
        req.allow_empty = false;
        req
    };
    repo.commits()
        .create(
            &grit_lib::commits::CommitOptions::new(req.clone(), CommitScope::AutoStage),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("commit");

    let post = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index after");
    assert_eq!(post.get(nfc.as_bytes(), 0).expect(nfc).oid, nfc_oid);
    assert_eq!(post.get(nfd.as_bytes(), 0).expect(nfd).oid, nfd_oid);
}

#[test]
fn collision_loser_auto_stage_commit_is_noop_for_nfc() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("grit-wt");
    fs::create_dir(&dest).expect("mkdir");
    let (repo, _) = Repository::init(
        &dest,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    enable_precompose(&repo, &dest);

    let nfc = "caf\u{00e9}.txt";
    let nfd = "cafe\u{0301}.txt";
    let tree = build_nfc_nfd_collision_tree(&repo);
    let _ = fs::remove_file(dest.join(nfc));
    let _ = fs::remove_file(dest.join(nfd));
    fs::write(dest.join(nfd), b"NFC\n").expect("shared file");
    repo.checkout()
        .checkout(None, &tree, &mut NullProgress, &CancelToken::never())
        .expect("checkout");

    let id = "Tester <t@example.com> 1700000000 +0000".to_owned();
    let mut seed = CommitRequest::new("seed".to_owned());
    seed.author = Some(id.parse().expect("sig"));
    seed.committer = Some(id.parse().expect("sig"));
    seed.allow_empty = false;
    repo.commits()
        .create(
            &grit_lib::commits::CommitOptions::new(seed, CommitScope::Staged),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("seed commit");

    let before = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let nfd_oid = before.get(nfd.as_bytes(), 0).expect(nfd).oid;
    let head_before = repo
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new("HEAD").expect("valid ref name"))
        .expect("head");

    let mut req = CommitRequest::new("e".to_owned());
    req.author = Some(id.parse().expect("sig"));
    req.committer = Some(id.parse().expect("sig"));
    req.allow_empty = false;
    let err = repo
        .commits()
        .create(
            &grit_lib::commits::CommitOptions::new(req, CommitScope::AutoStage),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect_err("auto-stage must not commit phantom loser changes");
    if aliases_nfc_and_nfd(tmp.path()) {
        // One shared dentry (APFS/HFS+): the loser holds its sibling's bytes, which is
        // collision drift rather than a clean tree, as with case-only collisions.
        match &err {
            Error::Commit(CommitError::WorktreeCaseCollision { paths }) => {
                assert_eq!(paths, &[nfd.to_owned(), nfc.to_owned()], "{err:?}");
            }
            other => panic!("expected WorktreeCaseCollision, got {other:?}"),
        }
    } else {
        assert!(
            matches!(
                err,
                Error::Commit(CommitError::NothingStagedToCommit { .. })
                    | Error::Commit(CommitError::NothingToCommit)
            ),
            "{err:?}"
        );
    }

    assert_eq!(
        repo.references()
            .resolve(grit_lib::refs::store::RefNameRef::new("HEAD").expect("valid ref name"))
            .expect("head"),
        head_before
    );
    let after = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index after");
    assert_eq!(after.get(nfd.as_bytes(), 0).expect(nfd).oid, nfd_oid);
}

#[test]
fn nfc_collision_loser_omitted_from_status_unstaged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("grit-wt");
    fs::create_dir(&dest).expect("mkdir");
    let (repo, _) = Repository::init(
        &dest,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    enable_precompose(&repo, &dest);
    let tree = build_nfc_nfd_collision_tree(&repo);
    repo.checkout()
        .checkout(None, &tree, &mut NullProgress, &CancelToken::never())
        .expect("checkout");

    let model = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status");
    let nfc = "caf\u{00e9}.txt";
    let nfd = "cafe\u{0301}.txt";
    let unstaged: Vec<&str> = model.unstaged.iter().map(|e| e.path()).collect();
    assert!(
        !unstaged.contains(&nfc),
        "dentry winner must not appear as unstaged: {unstaged:?}"
    );
    if aliases_nfc_and_nfd(tmp.path()) {
        // Both spellings open the one file holding the winner's bytes, so the loser's index
        // row really differs from the work tree; `git status` lists it as modified too.
        assert_eq!(unstaged, vec![nfd], "loser with sibling bytes is unstaged");
    } else {
        assert!(
            !unstaged.contains(&nfd),
            "collision loser must not appear as unstaged: {unstaged:?}"
        );
    }
}

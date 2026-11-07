//! `RevWalk::resolve` with `ResolveOptions`: every `Peel` x `IndexDwim` combination, and the
//! behavior of each `resolve_revision*` variant the options replaced.

use grit_lib::error::ObjectError;
use grit_lib::error::RefError;
use grit_lib::error::RevisionError;
use std::path::Path;

use grit_lib::error::Error;
use grit_lib::objects::hash::hash_object;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_parse_error::RevParseError;
use grit_lib::revwalk::{IndexDwim, MergeBaseMode, Peel, ResolveOptions, ResolvePurpose};

const PEELS: [Peel; 4] = [Peel::Any, Peel::Commit, Peel::Tree, Peel::Blob];
const INDEX_DWIM: [IndexDwim; 2] = [IndexDwim::On, IndexDwim::Off];

/// `main` with two commits, a side branch, an annotated tag, and a tracked `tracked.txt`.
struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repository,
    first: ObjectId,
    second: ObjectId,
    side: ObjectId,
    tag: ObjectId,
    tracked_blob: ObjectId,
}

fn stage_and_commit_worktree(repo: &Repository, message: &str, time: i64) -> ObjectId {
    repo.index().stage_worktree_paths(&[]).expect("stage");
    let ident = format!("A <a@example.com> {time} +0000");
    let req = grit_test_support::commit_request(message, &ident);
    repo.commits()
        .create_staged(&req, &mut NullProgress, &CancelToken::never())
        .expect("commit")
        .oid
}

fn write_worktree_file(path: &Path, content: &str) {
    std::fs::write(path, content).expect("write");
}

fn resolve_options_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path().join("w");
    let (repo, _) =
        Repository::init(&wt, &InitOptions::default().with_initial_branch("main")).expect("init");
    write_worktree_file(&wt.join("tracked.txt"), "tracked\n");
    let first = stage_and_commit_worktree(&repo, "first\n", 100);
    write_worktree_file(&wt.join("second.txt"), "second\n");
    let second = stage_and_commit_worktree(&repo, "second\n", 200);

    let tag_body =
        format!("object {first}\ntype commit\ntag v1\ntagger A <a@example.com> 300 +0000\n\nv1\n");
    let tag = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tag, tag_body.as_bytes())
        .expect("tag");
    grit_test_support::refs::set_ref(&repo, "refs/tags/v1", &tag).expect("tag ref");

    let side_body = format!(
        "tree {}\nparent {first}\nauthor A <a@example.com> 400 +0000\ncommitter A <a@example.com> 400 +0000\n\nside\n",
        root_tree_of(&repo, first)
    );
    let side = repo
        .odb()
        .write_loose_materialize(ObjectKind::Commit, side_body.as_bytes())
        .expect("side");
    grit_test_support::refs::set_ref(&repo, "refs/heads/side", &side).expect("side ref");

    let tracked_blob = hash_object(HashAlgo::Sha1, ObjectKind::Blob, b"tracked\n").expect("hash");
    Fixture {
        _dir: dir,
        repo,
        first,
        second,
        side,
        tag,
        tracked_blob,
    }
}

fn root_tree_of(repo: &Repository, commit: ObjectId) -> ObjectId {
    repo.revwalk().peel_to_tree(commit).expect("tree")
}

fn peel_dwim_options(peel: Peel, index_dwim: IndexDwim) -> ResolveOptions {
    ResolveOptions::default()
        .with_peel(peel)
        .with_index_dwim(index_dwim)
}

fn resolve_spec(
    repo: &Repository,
    spec: &str,
    options: &ResolveOptions,
) -> Result<ObjectId, Error> {
    repo.revwalk().resolve(spec, options)
}

/// A blob whose id shares the first four hex digits with `target`.
fn blob_sharing_prefix(target: ObjectId) -> Vec<u8> {
    let prefix = &target.to_hex()[..4];
    (0u64..)
        .map(|n| format!("blob {n}\n").into_bytes())
        .find(|data| {
            let oid = hash_object(HashAlgo::Sha1, ObjectKind::Blob, data).expect("hash");
            oid != target && oid.to_hex().starts_with(prefix)
        })
        .expect("colliding blob")
}

/// A one-entry tree whose id shares the first four hex digits with `target`.
fn tree_sharing_prefix(target: ObjectId, entry: ObjectId) -> Vec<u8> {
    let prefix = &target.to_hex()[..4];
    (0u64..)
        .map(|n| {
            let mut data = format!("100644 f{n}\0").into_bytes();
            data.extend_from_slice(entry.as_bytes());
            data
        })
        .find(|data| {
            let oid = hash_object(HashAlgo::Sha1, ObjectKind::Tree, data).expect("hash");
            oid != target && oid.to_hex().starts_with(prefix)
        })
        .expect("colliding tree")
}

#[test]
fn default_options_resolve_any_object_with_index_dwim() {
    let options = ResolveOptions::default();
    assert_eq!(options.peel, Peel::Any);
    assert_eq!(options.index_dwim, IndexDwim::On);
    assert_eq!(options.purpose, ResolvePurpose::Revision);
    assert_eq!(options, options.with_peel(Peel::Any));
}

#[test]
fn peel_and_index_dwim_matrix_on_a_branch_name() {
    let f = resolve_options_fixture();
    let tree = root_tree_of(&f.repo, f.second);
    for peel in PEELS {
        for index_dwim in INDEX_DWIM {
            let got = resolve_spec(&f.repo, "main", &peel_dwim_options(peel, index_dwim));
            match peel {
                Peel::Any | Peel::Commit => assert_eq!(got.ok(), Some(f.second), "{peel:?}"),
                Peel::Tree => assert_eq!(got.ok(), Some(tree), "{peel:?} {index_dwim:?}"),
                _ => assert!(
                    matches!(got, Err(Error::Object(ObjectError::NotFound { .. }))),
                    "{peel:?} {index_dwim:?}: a commit is not a blob, got {got:?}"
                ),
            }
        }
    }
}

#[test]
fn peel_and_index_dwim_matrix_on_a_tracked_path() {
    let f = resolve_options_fixture();
    for peel in PEELS {
        for index_dwim in INDEX_DWIM {
            let got = resolve_spec(&f.repo, "tracked.txt", &peel_dwim_options(peel, index_dwim));
            match index_dwim {
                IndexDwim::On => assert!(
                    matches!(
                        got,
                        Err(Error::Revision(RevisionError::Parse(
                            RevParseError::AmbiguousArgument { .. }
                        )))
                    ),
                    "{peel:?}: a tracked path is a possible pathspec, got {got:?}"
                ),
                _ => assert!(
                    matches!(
                        got,
                        Err(Error::Revision(RevisionError::Parse(
                            RevParseError::InvalidObjectName { .. }
                        )))
                    ),
                    "{peel:?}: without index DWIM it is a bad revision, got {got:?}"
                ),
            }
        }
    }
}

#[test]
fn peel_and_index_dwim_matrix_on_an_annotated_tag() {
    let f = resolve_options_fixture();
    let tree = root_tree_of(&f.repo, f.first);
    for index_dwim in INDEX_DWIM {
        let any =
            resolve_spec(&f.repo, "v1", &peel_dwim_options(Peel::Any, index_dwim)).expect("any");
        assert_eq!(any, f.tag, "Peel::Any keeps the tag object");
        let commit = resolve_spec(&f.repo, "v1", &peel_dwim_options(Peel::Commit, index_dwim))
            .expect("commit");
        assert_eq!(commit, f.first);
        let peeled_tree =
            resolve_spec(&f.repo, "v1", &peel_dwim_options(Peel::Tree, index_dwim)).expect("tree");
        assert_eq!(peeled_tree, tree);
        let blob = resolve_spec(&f.repo, "v1", &peel_dwim_options(Peel::Blob, index_dwim));
        assert!(
            matches!(blob, Err(Error::Object(ObjectError::NotFound { .. }))),
            "{blob:?}"
        );
    }
}

#[test]
fn peel_and_index_dwim_matrix_on_a_tree_path() {
    let f = resolve_options_fixture();
    for index_dwim in INDEX_DWIM {
        let blob = resolve_spec(
            &f.repo,
            "main:tracked.txt",
            &peel_dwim_options(Peel::Blob, index_dwim),
        );
        assert_eq!(blob.ok(), Some(f.tracked_blob), "{index_dwim:?}");
        let any = resolve_spec(
            &f.repo,
            "main:tracked.txt",
            &peel_dwim_options(Peel::Any, index_dwim),
        );
        assert_eq!(any.ok(), Some(f.tracked_blob), "{index_dwim:?}");
        let commit = resolve_spec(
            &f.repo,
            "main:tracked.txt",
            &peel_dwim_options(Peel::Commit, index_dwim),
        );
        assert!(
            matches!(commit, Err(Error::Ref(RefError::Invalid { .. }))),
            "{commit:?}"
        );
        let tree = resolve_spec(&f.repo, "main:", &peel_dwim_options(Peel::Tree, index_dwim));
        assert_eq!(
            tree.ok(),
            Some(root_tree_of(&f.repo, f.second)),
            "{index_dwim:?}"
        );
    }
}

/// `resolve_revision` became the default options.
#[test]
fn removed_resolve_revision_is_the_default() {
    let f = resolve_options_fixture();
    let default = ResolveOptions::default();
    assert_eq!(resolve_spec(&f.repo, "HEAD", &default).ok(), Some(f.second));
    assert_eq!(
        resolve_spec(&f.repo, "main~1", &default).ok(),
        Some(f.first)
    );
    assert_eq!(
        resolve_spec(&f.repo, ":tracked.txt", &default).ok(),
        Some(f.tracked_blob)
    );
    for unknown in ["tracked.txt", "no-such-name"] {
        let got = resolve_spec(&f.repo, unknown, &default);
        assert!(
            matches!(
                got,
                Err(Error::Revision(RevisionError::Parse(
                    RevParseError::AmbiguousArgument { .. }
                )))
            ),
            "{unknown}: {got:?}"
        );
    }
}

/// `resolve_revision_without_index_dwim` became `IndexDwim::Off`.
#[test]
fn removed_without_index_dwim_is_index_dwim_off() {
    let f = resolve_options_fixture();
    let off = ResolveOptions::default().with_index_dwim(IndexDwim::Off);
    assert_eq!(resolve_spec(&f.repo, "main", &off).ok(), Some(f.second));
    assert_eq!(
        resolve_spec(&f.repo, ":tracked.txt", &off).ok(),
        Some(f.tracked_blob)
    );
    for unknown in ["tracked.txt", "no-such-name"] {
        let got = resolve_spec(&f.repo, unknown, &off);
        assert!(
            matches!(
                got,
                Err(Error::Revision(RevisionError::Parse(
                    RevParseError::InvalidObjectName { .. }
                )))
            ),
            "{unknown}: {got:?}"
        );
    }
}

/// `resolve_revision_for_range_end` became `ResolvePurpose::RangeEnd`: ambiguous hex prefers a
/// commit; `Peel::Any` without it reports the ambiguity.
#[test]
fn removed_for_range_end_prefers_commits_for_ambiguous_hex() {
    let f = resolve_options_fixture();
    let blob = f
        .repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, &blob_sharing_prefix(f.second))
        .expect("blob");
    let prefix = &f.second.to_hex()[..4];
    let matches = f.repo.revwalk().abbrev_matches(prefix).expect("matches");
    assert_eq!(
        matches.len(),
        2,
        "{prefix} must name exactly the commit and the blob"
    );
    assert!(matches.contains(&blob));

    let range_end = ResolveOptions::default().with_purpose(ResolvePurpose::RangeEnd);
    assert_eq!(
        resolve_spec(&f.repo, prefix, &range_end).ok(),
        Some(f.second)
    );
    let plain = resolve_spec(&f.repo, prefix, &ResolveOptions::default());
    assert!(
        matches!(
            plain,
            Err(Error::Revision(RevisionError::Parse(
                RevParseError::AmbiguousShortObject { .. }
            )))
        ),
        "plain resolution must report the ambiguity, got {plain:?}"
    );
}

/// `resolve_revision_for_verify` and `resolve_revision_for_range_end_without_index_dwim`
/// became `IndexDwim::Off` + `ResolvePurpose::RangeEnd`.
#[test]
fn removed_for_verify_is_range_end_without_index_dwim() {
    let f = resolve_options_fixture();
    f.repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, &blob_sharing_prefix(f.second))
        .expect("blob");
    let verify = ResolveOptions::default()
        .with_index_dwim(IndexDwim::Off)
        .with_purpose(ResolvePurpose::RangeEnd);
    let prefix = &f.second.to_hex()[..4];
    assert_eq!(resolve_spec(&f.repo, prefix, &verify).ok(), Some(f.second));
    assert_eq!(resolve_spec(&f.repo, "v1", &verify).ok(), Some(f.tag));
    let path = resolve_spec(&f.repo, "tracked.txt", &verify);
    assert!(
        matches!(
            path,
            Err(Error::Revision(RevisionError::Parse(
                RevParseError::InvalidObjectName { .. }
            )))
        ),
        "--verify must not read the index, got {path:?}"
    );
}

/// `resolve_revision_as_commit` became `Peel::Commit`: ranges, tags and ambiguous hex.
#[test]
fn removed_as_commit_is_peel_commit() {
    let f = resolve_options_fixture();
    let commit = ResolveOptions::default().with_peel(Peel::Commit);
    assert_eq!(resolve_spec(&f.repo, "v1", &commit).ok(), Some(f.first));
    assert_eq!(
        resolve_spec(&f.repo, "main~1..main", &commit).ok(),
        Some(f.second)
    );
    assert_eq!(
        resolve_spec(&f.repo, "main..", &commit).ok(),
        Some(f.second)
    );
    assert_eq!(resolve_spec(&f.repo, "..side", &commit).ok(), Some(f.side));
    let merge_base = f
        .repo
        .revwalk()
        .merge_bases(&[f.second, f.side], MergeBaseMode::Best)
        .expect("merge base");
    assert_eq!(merge_base, vec![f.first]);
    assert_eq!(
        resolve_spec(&f.repo, "main...side", &commit).ok(),
        Some(f.first)
    );
    assert_eq!(
        resolve_spec(&f.repo, "side...", &commit).ok(),
        Some(f.first)
    );
    let tree = resolve_spec(&f.repo, "main^{tree}", &commit);
    assert!(
        matches!(tree, Err(Error::Ref(RefError::Invalid { .. }))),
        "{tree:?}"
    );

    f.repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, &blob_sharing_prefix(f.side))
        .expect("blob");
    let prefix = &f.side.to_hex()[..4];
    assert_eq!(resolve_spec(&f.repo, prefix, &commit).ok(), Some(f.side));
}

/// `resolve_revision_as_commit_without_index_dwim` became `Peel::Commit` + `IndexDwim::Off`.
#[test]
fn removed_as_commit_without_index_dwim() {
    let f = resolve_options_fixture();
    let commit_off = peel_dwim_options(Peel::Commit, IndexDwim::Off);
    assert_eq!(resolve_spec(&f.repo, "v1", &commit_off).ok(), Some(f.first));
    assert_eq!(
        resolve_spec(&f.repo, "main...side", &commit_off).ok(),
        Some(f.first)
    );
    let path = resolve_spec(&f.repo, "tracked.txt", &commit_off);
    assert!(
        matches!(
            path,
            Err(Error::Revision(RevisionError::Parse(
                RevParseError::InvalidObjectName { .. }
            )))
        ),
        "a tracked path is not a revision without index DWIM, got {path:?}"
    );
}

/// `resolve_revision_for_checkout_guess(.., false)` became `ResolvePurpose::CheckoutNoGuess`.
#[test]
fn removed_checkout_guess_is_checkout_no_guess() {
    let f = resolve_options_fixture();
    grit_test_support::refs::set_ref(&f.repo, "refs/remotes/origin/topic", &f.side)
        .expect("remote ref");
    let guess = resolve_spec(&f.repo, "topic", &ResolveOptions::default());
    assert_eq!(
        guess.ok(),
        Some(f.side),
        "a unique remote-tracking branch is guessed"
    );
    let no_guess = ResolveOptions::default().with_purpose(ResolvePurpose::CheckoutNoGuess);
    assert!(resolve_spec(&f.repo, "topic", &no_guess).is_err());
    assert_eq!(
        resolve_spec(&f.repo, "origin/topic", &no_guess).ok(),
        Some(f.side)
    );
    let commit_no_guess = no_guess.with_peel(Peel::Commit);
    assert!(resolve_spec(&f.repo, "topic", &commit_no_guess).is_err());
}

/// `resolve_revision_for_commit_tree_tree` became `Peel::Tree`: ambiguous hex prefers the tree,
/// and commits peel to their root tree.
#[test]
fn removed_commit_tree_tree_is_peel_tree() {
    let f = resolve_options_fixture();
    let blob = f
        .repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"tree target\n")
        .expect("blob");
    let tree = f
        .repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, &tree_sharing_prefix(blob, f.tracked_blob))
        .expect("tree");
    let prefix = &blob.to_hex()[..4];
    let peel_tree = ResolveOptions::default().with_peel(Peel::Tree);
    assert_eq!(resolve_spec(&f.repo, prefix, &peel_tree).ok(), Some(tree));
    assert_eq!(
        resolve_spec(&f.repo, "main", &peel_tree).ok(),
        Some(root_tree_of(&f.repo, f.second))
    );
}

/// `resolve_revision_for_patch_old_blob` became `Peel::Blob`: ambiguous hex prefers the blob.
#[test]
fn removed_patch_old_blob_is_peel_blob() {
    let f = resolve_options_fixture();
    let blob = f
        .repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, &blob_sharing_prefix(f.first))
        .expect("blob");
    let prefix = &f.first.to_hex()[..4];
    let peel_blob = ResolveOptions::default().with_peel(Peel::Blob);
    assert_eq!(resolve_spec(&f.repo, prefix, &peel_blob).ok(), Some(blob));
    assert_eq!(
        resolve_spec(&f.repo, "main:tracked.txt", &peel_blob).ok(),
        Some(f.tracked_blob)
    );
}

#[test]
fn explicit_peel_suffix_wins_over_the_kind_hint() {
    let f = resolve_options_fixture();
    f.repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, &blob_sharing_prefix(f.second))
        .expect("blob");
    let prefix = &f.second.to_hex()[..4];
    let commit_spec = format!("{prefix}^{{commit}}");
    let tree_spec = format!("{prefix}^{{tree}}");
    for peel in [Peel::Any, Peel::Commit, Peel::Blob] {
        let got = resolve_spec(
            &f.repo,
            &commit_spec,
            &peel_dwim_options(peel, IndexDwim::Off),
        );
        let expected = (peel != Peel::Blob).then_some(f.second);
        assert_eq!(got.ok(), expected, "{peel:?}");
    }
    let tree = resolve_spec(&f.repo, &tree_spec, &ResolveOptions::default());
    assert_eq!(tree.ok(), Some(root_tree_of(&f.repo, f.second)));
}

#[test]
fn peel_helpers_match_resolve_options() {
    let f = resolve_options_fixture();
    let walk = f.repo.revwalk();
    assert_eq!(walk.peel_to_commit(f.tag).ok(), Some(f.first));
    assert_eq!(
        walk.peel_to_tree(f.tag).ok(),
        Some(root_tree_of(&f.repo, f.first))
    );
    assert!(matches!(
        walk.peel_to_commit(f.tracked_blob),
        Err(Error::Ref(RefError::Invalid { .. }))
    ));
    assert!(matches!(
        walk.peel_to_tree(f.tracked_blob),
        Err(Error::Object(ObjectError::NotFound { .. }))
    ));
    let blob_at = walk.blob_at_path("v1:tracked.txt").expect("blob at path");
    assert_eq!(blob_at.oid, f.tracked_blob);
    assert_eq!(blob_at.path, "tracked.txt");
}

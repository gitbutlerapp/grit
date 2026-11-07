//! Change-id revisions on a repository whose primary object store is not the files backend.
//!
//! Commits grit makes carry a random `change-id` header, so a short change id names a
//! commit only through its stored header. On a custom primary the `objects/` directory
//! stays empty, which must not hide those headers from rev-parse, and the lookup must not
//! leave probe files under `objects/` either.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grit_lib::change_id::{find_commits_by_change_id_prefix, ChangeId};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::add::{StageOptions};
use grit_lib::porcelain::commit::{CommitRequest, CommitScope};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::{Peel, ResolveOptions};

const IDENT: &str = "A U Thor <author@example.com> 1700000000 +0000";

struct MemoryRepo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    git_dir: PathBuf,
    repo: Repository,
}

fn memory_repo() -> MemoryRepo {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    Repository::init(
        &root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let git_dir = root.join(".git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store)
            .with_work_tree(&root)),
    )
    .expect("open");
    MemoryRepo {
        _dir: dir,
        root,
        git_dir,
        repo,
    }
}

/// Write `file` and commit it, returning the commit and the change id grit stored in it.
fn commit_one_file(repo: &MemoryRepo, file: &str, contents: &str) -> (ObjectId, ChangeId) {
    fs::write(repo.root.join(file), contents).expect("write file");
repo.repo.index().stage(&StageOptions::default(), &mut NullProgress, &CancelToken::never(),
    )
    .expect("stage");
    let mut req = CommitRequest::new(format!("add {file}"));
    req.author = Some(IDENT.parse().expect("signature"));
    req.committer = Some(IDENT.parse().expect("signature"));
    let outcome = repo.repo.commits().create(&grit_lib::commits::CommitOptions::new(req.clone(), CommitScope::Staged), &mut NullProgress, &CancelToken::never(),
    )
    .expect("commit");
    let change_id = outcome.change_id.expect("grit stamps a change id");
    assert_ne!(
        change_id,
        ChangeId::derived_from_commit_id(&outcome.oid),
        "a stored id differs from the derived one"
    );
    (outcome.oid, change_id)
}

/// Every file under `dir`, relative to it.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path.strip_prefix(dir).expect("under dir").to_path_buf());
            }
        }
    }
    out
}

#[test]
fn short_change_id_resolves_through_a_memory_store() {
    let repo = memory_repo();
    let commits: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|name| commit_one_file(&repo, name, &format!("{name}\n")))
        .collect();

    for (oid, change_id) in &commits {
        let prefix = change_id.short(8);
        assert_eq!(
            repo.repo
                .revwalk()
                .resolve(prefix, &ResolveOptions::default().with_peel(Peel::Commit))
                .expect("resolve change-id prefix"),
            *oid,
            "change id {change_id}"
        );
        assert_eq!(
            repo.repo
                .revwalk()
                .resolve(change_id.as_str(), &ResolveOptions::default())
                .expect("full id"),
            *oid
        );
        let matches = find_commits_by_change_id_prefix(&repo.repo, prefix).expect("find");
        assert_eq!(matches.len(), 1, "{matches:?}");
        assert_eq!(matches[0].oid, *oid);
        assert_eq!(&matches[0].change_id, change_id);
    }
}

#[test]
fn derived_prefix_of_a_stamped_commit_does_not_resolve() {
    let repo = memory_repo();
    let (oid, _) = commit_one_file(&repo, "a", "a\n");
    let derived = ChangeId::derived_from_commit_id(&oid);
    let matches =
        find_commits_by_change_id_prefix(&repo.repo, derived.short(12)).expect("find derived");
    assert!(
        matches.is_empty(),
        "the stored header wins over the derived id: {matches:?}"
    );
}

#[test]
fn change_id_lookup_leaves_objects_dir_untouched() {
    let repo = memory_repo();
    let objects = repo.git_dir.join("objects");
    let before = files_under(&objects);
    let (_, first) = commit_one_file(&repo, "a", "a\n");
    repo.repo
        .revwalk()
        .resolve(
            first.short(8),
            &ResolveOptions::default().with_peel(Peel::Commit),
        )
        .expect("resolve");
    let (_, second) = commit_one_file(&repo, "b", "b\n");
    repo.repo
        .revwalk()
        .resolve(
            second.short(8),
            &ResolveOptions::default().with_peel(Peel::Commit),
        )
        .expect("resolve after a new commit");
    assert_eq!(files_under(&objects), before);
}

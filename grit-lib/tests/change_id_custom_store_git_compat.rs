//! Commits grit makes on a memory store, checked by the system `git`.
//!
//! The commits are written to a [`MemoryStore`] primary and their change ids resolved
//! there. Exported to the on-disk `objects/`, `git fsck` accepts them, `git` reads the
//! same `change-id` headers, and the files backend resolves each change id to the same
//! commit as the memory store did.

use std::fs;
use std::ops::ControlFlow;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::HashAlgo;
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::commit::{CommitRequest, CommitScope};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::{Peel, ResolveOptions};
use grit_test_support::git;

const IDENT: &str = "A U Thor <author@example.com> 1700000000 +0000";

#[test]
fn memory_store_change_ids_round_trip_through_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    Repository::init(
        &root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let git_dir = root.join(".git");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let memory = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_work_tree(&root)),
    )
    .expect("open memory repo");

    let mut commits = Vec::new();
    for name in ["a", "b", "c"] {
        fs::write(root.join(name), format!("{name}\n")).expect("write file");
        memory
            .index()
            .stage(
                &StageOptions::default(),
                &mut NullProgress,
                &CancelToken::never(),
            )
            .expect("stage");
        let mut req = CommitRequest::new(format!("add {name}"));
        req.author = Some(IDENT.parse().expect("signature"));
        req.committer = Some(IDENT.parse().expect("signature"));
        let outcome = memory
            .commits()
            .create(
                &grit_lib::commits::CommitOptions::new(req.clone(), CommitScope::Staged),
                &mut NullProgress,
                &CancelToken::never(),
            )
            .expect("commit");
        let change_id = outcome.change_id.expect("change id");
        assert_eq!(
            memory
                .revwalk()
                .resolve(
                    change_id.short(8),
                    &ResolveOptions::default().with_peel(Peel::Commit)
                )
                .expect("memory resolve"),
            outcome.oid
        );
        commits.push((outcome.oid, change_id));
    }

    let disk = Odb::new(&git_dir.join("objects"));
    let mut oids = Vec::new();
    store
        .for_each_object(&mut |oid| {
            oids.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("enumerate");
    for oid in oids {
        let obj = store.read(&oid).expect("read").expect("present");
        assert_eq!(disk.write(obj.kind, &obj.data).expect("export"), oid);
    }

    git(&root, &["fsck", "--full", "--strict"]);
    assert_eq!(git(&root, &["rev-list", "--count", "HEAD"]).trim(), "3");
    let files = Repository::open(&git_dir, &RepositoryOptions::empty().with_work_tree(&root))
        .expect("open files repo");
    for (oid, change_id) in &commits {
        let raw = git(&root, &["cat-file", "commit", &oid.to_hex()]);
        let header = raw
            .lines()
            .take_while(|line| !line.is_empty())
            .find_map(|line| line.strip_prefix("change-id "))
            .expect("git sees the change-id header");
        assert_eq!(header, change_id.as_str());
        assert_eq!(
            files
                .revwalk()
                .resolve(
                    change_id.short(8),
                    &ResolveOptions::default().with_peel(Peel::Commit)
                )
                .expect("files resolve"),
            *oid
        );
    }
}

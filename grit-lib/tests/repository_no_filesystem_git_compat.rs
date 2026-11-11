//! Objects move between the system `git` and a bare repository whose object store, ref
//! store and config are all injected: a local fetch reads a repository `git` wrote, and
//! a bundle written into memory clones and passes `git fsck --strict`.

use grit_lib::objects::FileMode;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::bundle::BundleSpec;
use grit_lib::config::environment::{ConfigOverride, Environment, RepositoryOptions};
use grit_lib::ident::Signature;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::{HashAlgo, NewCommit, ObjectKind, TreeEntry};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::store::{MemoryRefStore, RefName};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::FetchOptions;
use grit_lib::remote::Remote;
use grit_lib::repo::Repository;
use grit_lib::revwalk::ResolveOptions;
use grit_test_support::git;
use time::OffsetDateTime;

fn open_injected(git_dir: &Path) -> Repository {
    let mut env = Environment::empty();
    env.git_config_nosystem = Some("1".to_owned());
    env.push_config_override(
        &ConfigOverride::new("user.name", Some("Injected".to_owned())).expect("override"),
    );
    Repository::open(
        git_dir,
        &RepositoryOptions::with_environment(env)
            .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
            .with_ref_store(Arc::new(MemoryRefStore::new()))
            .with_initial_branch("main"),
    )
    .expect("open fully injected repository")
}

#[test]
fn git_history_fetched_into_memory_round_trips_through_an_in_memory_bundle() {
    let upstream = tempfile::tempdir().expect("upstream");
    git(upstream.path(), &["init", "-q", "-b", "main", "."]);
    fs::write(upstream.path().join("a.txt"), "one\n").expect("write");
    git(upstream.path(), &["add", "a.txt"]);
    git(upstream.path(), &["commit", "-q", "-m", "first"]);
    fs::write(upstream.path().join("a.txt"), "one\ntwo\n").expect("write");
    git(upstream.path(), &["commit", "-q", "-am", "second"]);
    let upstream_tip = git(upstream.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    let scratch = tempfile::tempdir().expect("scratch");
    let git_dir = scratch.path().join("never-created.git");
    let repo = open_injected(&git_dir);

    let remote = Remote::from_url(upstream.path().to_str().expect("utf-8")).expect("remote");
    let mut fetch = FetchOptions::default();
    fetch.refspecs = vec!["+refs/heads/main:refs/heads/main".to_owned()];
    repo.remotes()
        .fetch(&remote, fetch, &mut NoProgress, &CancelToken::never(), None)
        .expect("fetch from a repository git wrote");
    let fetched = repo
        .revwalk()
        .resolve("main", &ResolveOptions::default())
        .expect("main");
    assert_eq!(fetched.to_hex(), upstream_tip);

    let blob = repo
        .objects()
        .write(ObjectKind::Blob, b"written in memory\n")
        .expect("blob");
    let mut entries = grit_lib::objects::parse_tree(
        &repo
            .objects()
            .read(&repo.objects().commit_tree(&fetched).expect("tree"))
            .expect("read tree")
            .data,
    )
    .expect("parse tree");
    entries.push(TreeEntry::new(b"memory.txt", FileMode::REGULAR, blob));
    let tree = repo.objects().write_tree(entries).expect("tree");
    let who = Signature::new(
        "Ada",
        "ada@example.com",
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
    )
    .expect("signature");
    let tip = repo
        .objects()
        .write_commit(
            &NewCommit::new(tree, who.clone(), who, "in memory").with_parents(vec![fetched]),
        )
        .expect("commit");
    repo.references()
        .compare_and_swap(
            &RefName::new("refs/heads/main").expect("valid ref name"),
            &fetched,
            &tip,
        )
        .expect("advance main");

    let mut bundle = Vec::new();
    repo.bundles()
        .write_to(
            &BundleSpec {
                include: vec![(tip, "refs/heads/main".into())],
                ..BundleSpec::default()
            },
            &mut bundle,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("bundle");
    assert!(!git_dir.exists(), "{} was created", git_dir.display());
    assert_eq!(
        fs::read_dir(scratch.path()).expect("read scratch").count(),
        0,
        "nothing may be created next to the git directory"
    );

    let outside = tempfile::tempdir().expect("outside");
    let bundle_path = outside.path().join("memory.bundle");
    fs::write(&bundle_path, &bundle).expect("write bundle");
    git(
        outside.path(),
        &["clone", "-q", "--bare", "memory.bundle", "clone.git"],
    );
    let clone = outside.path().join("clone.git");
    git(&clone, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(
        git(&clone, &["rev-parse", "refs/heads/main"]).trim(),
        tip.to_hex()
    );
    assert_eq!(
        git(&clone, &["show", "refs/heads/main:memory.txt"]),
        "written in memory\n"
    );
    assert_eq!(
        git(&clone, &["rev-list", "--count", "refs/heads/main"]).trim(),
        "3"
    );
}

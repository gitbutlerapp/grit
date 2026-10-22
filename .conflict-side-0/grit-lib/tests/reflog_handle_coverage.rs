//! Public API coverage for [`RepositoryReflogs`](grit_lib::repo_handles::RepositoryReflogs).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::append_reflog;
use grit_lib::refs::store::RefName;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::{ExpireOptions, RepositoryReflogs};

fn open_repo(path: &std::path::Path) -> Repository {
    Repository::open(
        &path.join(".git"),
        &RepositoryOptions::empty().with_work_tree(path),
    )
    .expect("open")
}

#[test]
fn repository_reflogs_public_methods() {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let repo = open_repo(dir.path());
    let handle = repo.reflogs();
    assert!(std::ptr::eq(handle.repository(), &repo));

    let refname = "refs/heads/main";
    let git_dir = repo.git_dir();
    append_reflog(
        git_dir,
        refname,
        &grit_lib::objects::ObjectId::zero(),
        &grit_lib::objects::ObjectId::zero(),
        "T <t@example.com> 1700000000 +0000",
        "cov",
        true,
    )
    .expect("append");

    let refs = handle.refs().expect("refs");
    assert!(refs.iter().any(|r| r.as_str() == refname));

    let name = RefName::try_from(refname).expect("valid");
    let entries = handle.entries(&name).expect("entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].index, 0);

    handle.delete(&name, &[0], false).expect("delete");
    assert!(handle
        .entries(&name)
        .expect("entries after delete")
        .is_empty());

    append_reflog(
        git_dir,
        refname,
        &grit_lib::objects::ObjectId::zero(),
        &grit_lib::objects::ObjectId::zero(),
        "T <t@example.com> 1000000000 +0000",
        "old",
        true,
    )
    .expect("re-append");

    let outcome = handle
        .expire(&ExpireOptions {
            now: 1_700_000_000,
            expire: Some("now".into()),
            expire_unreachable: Some("never".into()),
            all: false,
        })
        .expect("expire");
    assert!(outcome.entries_pruned >= 1);

    let _ = std::any::type_name::<RepositoryReflogs<'_>>();
}

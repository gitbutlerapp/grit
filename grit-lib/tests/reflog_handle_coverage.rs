//! Public API coverage for [`RepositoryReflogs`](grit_lib::repo_handles::RepositoryReflogs).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
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
    grit_test_support::refs::at(git_dir)
        .reflogs()
        .append(
            refname,
            &grit_lib::refs::reflog::ReflogEntry {
                old_oid: grit_lib::objects::ObjectId::zero(),
                new_oid: grit_lib::objects::ObjectId::zero(),
                identity: ("T <t@example.com> 1700000000 +0000").to_string(),
                message: ("cov").to_string(),
            },
            grit_lib::repo_handles::ReflogCreate::Always,
        )
        .expect("append");

    let refs = handle.list_reflog_refs().expect("refs");
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

    grit_test_support::refs::at(git_dir)
        .reflogs()
        .append(
            refname,
            &grit_lib::refs::reflog::ReflogEntry {
                old_oid: grit_lib::objects::ObjectId::zero(),
                new_oid: grit_lib::objects::ObjectId::zero(),
                identity: ("T <t@example.com> 1000000000 +0000").to_string(),
                message: ("old").to_string(),
            },
            grit_lib::repo_handles::ReflogCreate::Always,
        )
        .expect("re-append");

    let outcome = handle
        .expire(
            &ExpireOptions::new(1_700_000_000)
                .with_expire(Some("now".into()))
                .with_expire_unreachable(Some("never".into())),
        )
        .expect("expire");
    assert!(outcome.entries_pruned >= 1);
    assert!(!outcome.dry_run);
    assert!(outcome
        .refs
        .iter()
        .any(|r| r.ref_name == refname && r.pruned >= 1));

    let _ = std::any::type_name::<RepositoryReflogs<'_>>();
}

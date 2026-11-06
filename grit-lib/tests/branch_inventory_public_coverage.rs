//! Coverage for branch inventory public API.

#![allow(clippy::unwrap_used)]

use grit_lib::error::Error;
use grit_lib::refs::branch_tracking::{
    list_branches, upstream_status, BranchDetail, BranchListQuery, BranchScope,
};
use grit_lib::refs::listing::{RefQuery, RefSort, SortKey};
use grit_lib::repo::{InitOptions, Repository};

#[test]
fn branch_inventory_public_api_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let query = BranchListQuery {
        scope: BranchScope::Local,
        refs: RefQuery::default(),
    };
    let names = list_branches(&repo, &query, BranchDetail::Names).unwrap();
    assert!(names.iter().all(|b| b.subject.is_none()));
    assert!(upstream_status(&repo, "main").unwrap().is_none());
    let _ = list_branches(
        &repo,
        &BranchListQuery {
            scope: BranchScope::All,
            refs: RefQuery::default(),
        },
        BranchDetail::Full,
    )
    .unwrap();

    let err = list_branches(
        &repo,
        &BranchListQuery {
            scope: BranchScope::Local,
            refs: RefQuery {
                sort: RefSort {
                    key: SortKey::Date,
                    reverse: false,
                },
                ..RefQuery::default()
            },
        },
        BranchDetail::Names,
    )
    .unwrap_err();
    assert!(matches!(err, Error::BranchListNamesDateSort));
}

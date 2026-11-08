//! Coverage for branch inventory public API.

#![allow(clippy::unwrap_used)]

use grit_lib::refs::branch_tracking::{BranchDetail, BranchListQuery, BranchScope};
use grit_lib::refs::listing::{RefQuery, RefSort, SortKey};
use grit_lib::repo::{InitOptions, Repository};

#[test]
fn branch_inventory_public_api_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let query = BranchListQuery {
        scope: BranchScope::Local,
        refs: RefQuery::default(),
    };
    let names = repo.branches().list(&query, BranchDetail::Names).unwrap();
    assert!(names.iter().all(|b| b.subject.is_none()));
    assert!(repo.branches().upstream_status("main").unwrap().is_none());
    let _ = repo
        .branches()
        .list(
            &BranchListQuery {
                scope: BranchScope::All,
                refs: RefQuery::default(),
            },
            BranchDetail::Full,
        )
        .unwrap();

    let date_sorted_names = repo
        .branches()
        .list(
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
        .unwrap();
    assert!(date_sorted_names.is_empty());
}

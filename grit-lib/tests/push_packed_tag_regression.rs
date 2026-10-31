//! Regression for issue #2366: `push --tags` must not replace annotated tag objects with commits.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::refs::gc::collect_referenced_object_roots;
use grit_lib::refs::list_refs;
use grit_lib::remote::push_plan::{
    follow_tag_push_specs, resolve_push, PushPlanContext, PushRequest,
};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{push_local, PushOptions};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};
use std::collections::HashSet;
use std::path::Path;

fn open_repo(path: &Path) -> Repository {
    Repository::discover(Some(path), &RepositoryOptions::empty()).expect("open repo")
}

fn load_config(repo: &Repository) -> ConfigSet {
    ConfigSet::load(&Environment::empty(), Some(repo.git_dir()), true).expect("config")
}

fn ctx_from_bare(bare: &Path) -> PushPlanContext {
    let out = git_cmd(&["for-each-ref", "--format=%(refname) %(objectname)"])
        .in_dir(bare)
        .exec();
    assert!(out.ok(), "for-each-ref: {}", out.stderr);
    let mut remote_ref_names = HashSet::new();
    let mut remote_branch_tips = HashSet::new();
    for line in out.stdout.lines() {
        let Some((name, hex)) = line.split_once(' ') else {
            continue;
        };
        if name.ends_with("^{}") {
            continue;
        }
        remote_ref_names.insert(name.to_owned());
        if name.starts_with("refs/heads/") {
            remote_branch_tips.insert(ObjectId::from_hex(hex).expect("oid"));
        }
    }
    PushPlanContext {
        remote_ref_names,
        remote_branch_tips,
        remote_tips: None,
    }
}

#[test]
fn packed_annotated_tags_push_is_up_to_date_and_preserves_remote_tag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    git(&local, &["tag", "-a", "-m", "release v1", "v1"]);
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&local, &["push", "-q", "origin", "main", "v1"]);
    git(&local, &["pack-refs", "--all"]);

    let tag_oid: ObjectId = git(&local, &["rev-parse", "refs/tags/v1"])
        .trim()
        .parse()
        .unwrap();
    let listed: Vec<_> = list_refs(&local.join(".git"), "refs/tags/")
        .expect("list_refs")
        .into_iter()
        .filter(|(name, _)| name == "refs/tags/v1")
        .collect();
    assert_eq!(listed, vec![("refs/tags/v1".to_owned(), tag_oid)]);

    let roots = collect_referenced_object_roots(&local.join(".git")).expect("roots");
    assert!(roots.contains(&tag_oid));

    let repo = open_repo(&local);
    let config = load_config(&repo);
    let ctx = ctx_from_bare(&bare);
    let specs = resolve_push(
        &repo,
        &config,
        &PushRequest {
            tags: true,
            remote: Some("origin".into()),
            ..Default::default()
        },
        &ctx,
    )
    .expect("plan")
    .specs;
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].src, Some(tag_oid));
    assert!(follow_tag_push_specs(&repo, &specs, &ctx)
        .expect("follow")
        .is_empty());

    let outcome = push_local(
        &local.join(".git"),
        &bare,
        &specs,
        &PushOptions::default(),
        &grit_lib::progress::NeverCancel,
    )
    .expect("push --tags");
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.results[0].status, PushRefStatus::UpToDate);

    let remote_kind = git(&bare, &["cat-file", "-t", "refs/tags/v1"]);
    assert_eq!(remote_kind.trim(), "tag");
    let remote_oid = git(&bare, &["rev-parse", "refs/tags/v1"]).trim().to_owned();
    assert_eq!(remote_oid, tag_oid.to_hex());
}

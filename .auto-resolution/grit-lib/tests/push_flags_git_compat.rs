//! `push -u`, `--atomic`, `--push-option`, and `--follow-tags` vs system `git`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::remote::push_plan::{
    follow_tag_push_specs, resolve_push, PushPlanContext, PushRequest,
};
use grit_lib::remote::transfer::{push_local, PushOptions, PushRefSpec};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

fn git_config(repo: &Path, key: &str) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["config", "--get", key])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git config");
    assert!(out.status.success(), "missing config {key}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn open_repo(path: &Path) -> Repository {
    Repository::discover(Some(path), &RepositoryOptions::empty()).expect("open")
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
    }
}

#[test]
fn push_set_upstream_writes_same_config_as_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&bare).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["remote", "add", "origin", bare.to_str().unwrap()]);

    git(&local, &["push", "-u", "origin", "main"]);

    let git_remote = git_config(&local, "branch.main.remote");
    let git_merge = git_config(&local, "branch.main.merge");

    // Reset upstream and push with grit + apply_set_upstream.
    git(&local, &["config", "--unset", "branch.main.remote"]);
    git(&local, &["config", "--unset", "branch.main.merge"]);

    let repo = open_repo(&local);
    let config = load_config(&repo);
    let ctx = ctx_from_bare(&bare);
    let plan = resolve_push(
        &repo,
        &config,
        &PushRequest {
            set_upstream: true,
            ..PushRequest {
                remote: Some("origin".into()),
                refspecs: vec!["main".into()],
                ..Default::default()
            }
        },
        &ctx,
    )
    .expect("plan");
    let out = push_local(
        repo.git_dir(),
        &bare,
        &plan.specs,
        &PushOptions {
            tracking_remote: Some("origin".into()),
            ..Default::default()
        },
    )
    .expect("push");
    grit_lib::remote::push_plan::apply_set_upstream_after_push(
        &repo,
        "origin",
        true,
        false,
        &plan.specs,
        &out.results,
    )
    .expect("upstream");

    assert_eq!(git_config(&local, "branch.main.remote"), git_remote);
    assert_eq!(git_config(&local, "branch.main.merge"), git_merge);
}

#[test]
fn atomic_push_aborts_all_updates_on_bare_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&bare).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("a"), "1\n").unwrap();
    git(&local, &["add", "a"]);
    git(&local, &["commit", "-q", "-m", "m1"]);
    git(&local, &["branch", "side"]);
    std::fs::write(local.join("b"), "2\n").unwrap();
    git(&local, &["add", "b"]);
    git(&local, &["commit", "-q", "-m", "m2"]);
    git(&local, &["checkout", "-q", "main"]);
    std::fs::write(local.join("c"), "3\n").unwrap();
    git(&local, &["add", "c"]);
    git(&local, &["commit", "-q", "-m", "m3"]);
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&local, &["push", "-q", "origin", "main"]);

    let side_oid = git_cmd(&["rev-parse", "side"])
        .in_dir(&local)
        .exec()
        .stdout
        .trim()
        .to_owned();
    fn spec(src: &str, dst: &str) -> PushRefSpec {
        PushRefSpec {
            src: Some(ObjectId::from_hex(src).unwrap()),
            src_ref: None,
            dst: dst.to_owned(),
            force: false,
            delete: false,
            ..Default::default()
        }
    }

    let good = spec(&side_oid, "refs/heads/atomic-ok");
    let bad = spec(&side_oid, "refs/heads/main");

    let atomic = PushOptions {
        atomic: true,
        ..Default::default()
    };
    push_local(
        local.join(".git").as_path(),
        &bare,
        &[good.clone(), bad],
        &atomic,
    )
    .expect("atomic push returns outcome");

    assert!(
        !git_cmd(&["show-ref", "--verify", "refs/heads/atomic-ok"])
            .in_dir(&bare)
            .exec()
            .ok(),
        "atomic push must not create the good ref"
    );

    // Without atomic, partial success: good ref lands, bad stays rejected.
    push_local(
        local.join(".git").as_path(),
        &bare,
        &[good, spec(&side_oid, "refs/heads/main")],
        &PushOptions::default(),
    )
    .expect("non-atomic");
    assert!(git_cmd(&["show-ref", "--verify", "refs/heads/atomic-ok"])
        .in_dir(&bare)
        .exec()
        .ok());
}

#[test]
fn follow_tags_matches_git_push_follow_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&bare).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    git(&local, &["tag", "light"]);
    git(&local, &["tag", "-a", "-m", "ann-reach", "ann-reach"]);
    git(&local, &["branch", "orphan"]);
    git(&local, &["checkout", "-q", "orphan"]);
    std::fs::write(local.join("g"), "2\n").unwrap();
    git(&local, &["add", "g"]);
    git(&local, &["commit", "-q", "-m", "orphan"]);
    git(&local, &["tag", "-a", "-m", "ann-unreach", "ann-unreach"]);
    git(&local, &["checkout", "-q", "main"]);
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["remote", "add", "origin", bare.to_str().unwrap()]);

    let repo = open_repo(&local);
    let config = load_config(&repo);
    let ctx = ctx_from_bare(&bare);
    let branch_specs = resolve_push(
        &repo,
        &config,
        &PushRequest {
            remote: Some("origin".into()),
            refspecs: vec!["main".into()],
            ..Default::default()
        },
        &ctx,
    )
    .expect("plan")
    .specs;

    let grit_tags: HashSet<String> = follow_tag_push_specs(&repo, &branch_specs, &ctx)
        .expect("follow")
        .into_iter()
        .map(|s| s.dst)
        .collect();

    git(&local, &["push", "--follow-tags", "origin", "main"]);
    let git_out = git_cmd(&["for-each-ref", "--format=%(refname)", "refs/tags/"])
        .in_dir(&bare)
        .exec();
    let git_tags: HashSet<String> = git_out.stdout.lines().map(str::to_owned).collect();

    assert_eq!(grit_tags, git_tags);
    assert!(git_tags.contains("refs/tags/ann-reach"));
    assert!(!git_tags.contains("refs/tags/light"));
    assert!(!git_tags.contains("refs/tags/ann-unreach"));
}

#[test]
fn follow_tags_on_up_to_date_branch_includes_older_annotated_tag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&bare).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    git(&local, &["tag", "-a", "-m", "old", "old-tag"]);
    std::fs::write(local.join("f"), "2\n").unwrap();
    git(&local, &["commit", "-am", "c2"]);
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&local, &["push", "-q", "origin", "main"]);

    let repo = open_repo(&local);
    let config = load_config(&repo);
    let ctx = ctx_from_bare(&bare);
    let branch_specs = resolve_push(
        &repo,
        &config,
        &PushRequest {
            remote: Some("origin".into()),
            refspecs: vec!["main".into()],
            ..Default::default()
        },
        &ctx,
    )
    .expect("plan")
    .specs;

    let grit_tags: HashSet<String> = follow_tag_push_specs(&repo, &branch_specs, &ctx)
        .expect("follow")
        .into_iter()
        .map(|s| s.dst)
        .collect();

    git(&local, &["push", "--follow-tags", "origin", "main"]);
    let git_out = git_cmd(&["for-each-ref", "--format=%(refname)", "refs/tags/"])
        .in_dir(&bare)
        .exec();
    let git_tags: HashSet<String> = git_out.stdout.lines().map(str::to_owned).collect();

    assert_eq!(grit_tags, git_tags);
    assert!(git_tags.contains("refs/tags/old-tag"));
}

#[test]
fn push_local_rejects_push_options() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&bare).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);

    let main_oid = git_cmd(&["rev-parse", "HEAD"]).in_dir(&local).exec().stdout;
    let main_oid = ObjectId::from_hex(main_oid.trim()).unwrap();
    let spec = PushRefSpec {
        src: Some(main_oid),
        src_ref: Some("refs/heads/main".into()),
        dst: "refs/heads/main".into(),
        ..Default::default()
    };
    let err = push_local(
        local.join(".git").as_path(),
        &bare,
        &[spec],
        &PushOptions {
            push_options: vec!["ci.skip".into()],
            ..Default::default()
        },
    )
    .expect_err("local push must reject push options");
    assert!(
        matches!(err, grit_lib::error::Error::PushOptionsUnsupported),
        "expected PushOptionsUnsupported, got {err:?}"
    );
    assert!(
        !git_cmd(&["show-ref", "--verify", "refs/heads/main"])
            .in_dir(&bare)
            .exec()
            .ok(),
        "bare ref must not be created"
    );
}

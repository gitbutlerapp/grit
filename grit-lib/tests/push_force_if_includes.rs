//! `--force-if-includes` with tracking force-with-lease vs system `git push`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::progress::CancelToken;
use grit_lib::remote::push_plan::{ForceMode, LeaseExpectation, PushPlanContext, PushRequest};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushOutcome};
use grit_lib::remote::Remote;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn open_repo(work: &Path) -> Repository {
    Repository::discover(Some(work), &RepositoryOptions::empty()).expect("open")
}

fn load_config(repo: &Repository) -> ConfigSet {
    ConfigSet::load(
        &Environment::empty(),
        Some(repo.git_dir()),
        &crate::config::LoadConfigOptions::standard(&Environment::empty(), Some(repo.git_dir())),
    )
    .expect("config")
}

fn push_plan_context(bare: &Path) -> PushPlanContext {
    let out = Command::new("git")
        .current_dir(bare)
        .args(["for-each-ref", "--format=%(refname)"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("for-each-ref");
    let names: std::collections::HashSet<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    PushPlanContext::default().with_remote_ref_names(names)
}

fn grit_push_request(work: &Path, bare: &Path, request: PushRequest) -> PushOutcome {
    let repo = open_repo(work);
    let config = load_config(&repo);
    let ctx = push_plan_context(bare);
    let plan = repo.remotes().resolve_push(&request, &ctx).expect("plan");
    Remote::from_config(&config, "origin")
        .expect("remote")
        .push(
            &repo,
            &plan.specs,
            PushOptions::default().with_tracking_remote(Some("origin".into())),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
            None,
        )
        .expect("push")
}

fn git_push_includes(work: &Path) -> bool {
    Command::new("git")
        .current_dir(work)
        .args([
            "push",
            "--force-with-lease",
            "--force-if-includes",
            "-q",
            "origin",
            "main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn remote_main(bare: &Path) -> ObjectId {
    let hex = git(bare, &["rev-parse", "refs/heads/main"]);
    ObjectId::from_hex(hex.trim()).expect("oid")
}

#[test]
fn background_fetch_plus_amend_rejects_until_remote_tip_integrated() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    let client = tmp.path().join("client");
    std::fs::create_dir_all(&client).unwrap();
    git(
        &tmp.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    git(&client, &["init", "-q", "-b", "main"]);
    git(
        &client,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    std::fs::write(client.join("f.txt"), "1\n").unwrap();
    git(&client, &["add", "f.txt"]);
    git(&client, &["commit", "-q", "-m", "c1"]);
    git(&client, &["push", "-q", "origin", "main"]);
    git(&client, &["fetch", "-q", "origin"]);

    let stale = remote_main(&bare);
    let tree = git(
        &bare,
        &["rev-parse", &format!("{}^{{tree}}", stale.to_hex())],
    );
    let other = git(
        &bare,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            stale.to_hex().as_str(),
            "-m",
            "other",
        ],
    );
    git(&bare, &["update-ref", "refs/heads/main", other.trim()]);
    git(&client, &["fetch", "-q", "origin"]);
    std::fs::write(client.join("g.txt"), "2\n").unwrap();
    git(&client, &["add", "g.txt"]);
    git(&client, &["commit", "-q", "--amend", "--no-edit"]);

    let rejected = grit_push_request(
        &client,
        &bare,
        PushRequest::default()
            .with_remote(Some("origin".into()))
            .with_force(ForceMode::Lease(LeaseExpectation::Tracking))
            .with_force_if_includes(true),
    );
    assert_eq!(
        rejected.results[0].status,
        PushRefStatus::RejectRemoteUpdated
    );
    assert!(!git_push_includes(&client));
    assert_eq!(
        remote_main(&bare),
        ObjectId::from_hex(other.trim()).unwrap()
    );

    git(&client, &["rebase", "origin/main"]);
    let ok = grit_push_request(
        &client,
        &bare,
        PushRequest::default()
            .with_remote(Some("origin".into()))
            .with_force(ForceMode::Lease(LeaseExpectation::Tracking))
            .with_force_if_includes(true),
    );
    assert_eq!(ok.results[0].status, PushRefStatus::Ok);
    assert!(git_push_includes(&client));
}

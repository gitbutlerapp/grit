//! `resolve_push` force-with-lease / force vs system `git push` on the same bare remote.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push_plan::{
    resolve_push, ForceMode, LeaseExpectation, PushPlanContext, PushRequest,
};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushOutcome};
use grit_lib::remote::Remote;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_try(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", rev])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("oid")
}

fn fsck_clean(bare: &Path) {
    let out = Command::new("git")
        .current_dir(bare)
        .args(["fsck", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        out.status.success(),
        "fsck: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn open_repo(work: &Path) -> Repository {
    Repository::discover(Some(work), &RepositoryOptions::empty()).expect("open")
}

fn load_config(repo: &Repository) -> ConfigSet {
    ConfigSet::load(&Environment::empty(), Some(repo.git_dir()), true).expect("config")
}

fn push_plan_context(bare: &Path) -> PushPlanContext {
    let out = Command::new("git")
        .current_dir(bare)
        .args(["for-each-ref", "--format=%(refname) %(objectname)"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("for-each-ref");
    let mut remote_ref_names = HashSet::new();
    let mut remote_branch_tips = HashSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
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

struct Fixture {
    _tmp: tempfile::TempDir,
    bare: PathBuf,
    grit_client: PathBuf,
    git_client: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("remote.git");
        let grit_client = tmp.path().join("grit-client");
        let git_client = tmp.path().join("git-client");
        std::fs::create_dir_all(&grit_client).unwrap();
        std::fs::create_dir_all(&git_client).unwrap();
        std::fs::create_dir_all(&bare).unwrap();
        git(&grit_client, &["init", "-q", "-b", "main"]);
        git(&git_client, &["init", "-q", "-b", "main"]);
        git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
        let bare_s = bare.to_str().unwrap();
        git(&grit_client, &["remote", "add", "origin", bare_s]);
        git(&git_client, &["remote", "add", "origin", bare_s]);
        Self {
            _tmp: tmp,
            bare,
            grit_client,
            git_client,
        }
    }

    fn seed_commits(&self) {
        for client in [&self.grit_client, &self.git_client] {
            std::fs::write(client.join("f.txt"), "1\n").unwrap();
            git(client, &["add", "f.txt"]);
            git(client, &["commit", "-q", "-m", "c1"]);
        }
        git(&self.grit_client, &["push", "-q", "origin", "main"]);
        git(&self.git_client, &["push", "-q", "origin", "main"]);
        git(&self.grit_client, &["fetch", "-q", "origin"]);
        git(&self.git_client, &["fetch", "-q", "origin"]);
    }

    fn amend_main(&self) {
        for client in [&self.grit_client, &self.git_client] {
            std::fs::write(client.join("amend.txt"), "a\n").unwrap();
            git(client, &["add", "amend.txt"]);
            git(client, &["commit", "-q", "--amend", "--no-edit"]);
        }
    }
}

fn grit_push_request(work: &Path, bare: &Path, request: PushRequest) -> PushOutcome {
    let repo = open_repo(work);
    let config = load_config(&repo);
    let ctx = push_plan_context(bare);
    let plan = resolve_push(&repo, &config, &request, &ctx).expect("plan");
    Remote::from_config(&config, "origin")
        .expect("remote")
        .push(
            &repo,
            &plan.specs,
            PushOptions {
                tracking_remote: Some("origin".into()),
                ..Default::default()
            },
            &mut NoProgress,
            None,
        )
        .expect("push")
}

fn grit_push(work: &Path, bare: &Path, force: ForceMode, refspecs: &[&str]) -> PushOutcome {
    grit_push_request(
        work,
        bare,
        PushRequest {
            remote: Some("origin".into()),
            refspecs: refspecs.iter().map(|s| (*s).to_owned()).collect(),
            force,
            ..Default::default()
        },
    )
}

fn git_push_lease(work: &Path, lease: Option<&str>) -> bool {
    match lease {
        None => git_try(
            work,
            &["push", "--force-with-lease", "-q", "origin", "main"],
        ),
        Some(flag) => git_try(work, &["push", flag, "-q", "origin", "main"]),
    }
}

fn git_push_force(work: &Path) -> bool {
    git_try(work, &["push", "--force", "-q", "origin", "main"])
}

fn git_push_lease_delete(work: &Path) -> bool {
    git_try(
        work,
        &[
            "push",
            "--force-with-lease",
            "-q",
            "--delete",
            "origin",
            "main",
        ],
    )
}

fn remote_tip(bare: &Path) -> ObjectId {
    rev_parse(bare, "refs/heads/main")
}

#[test]
fn file_lease_succeeds_when_tracking_matches() {
    let fx = Fixture::new();
    fx.seed_commits();
    fx.amend_main();
    let grit_out = grit_push(
        &fx.grit_client,
        &fx.bare,
        ForceMode::Lease(LeaseExpectation::Tracking),
        &[],
    );
    assert_eq!(grit_out.results[0].status, PushRefStatus::Ok);
    assert!(git_push_lease(&fx.git_client, None));
    let tip = remote_tip(&fx.bare);
    assert_eq!(tip, rev_parse(&fx.grit_client, "HEAD"));
    assert_eq!(tip, rev_parse(&fx.git_client, "HEAD"));
    fsck_clean(&fx.bare);
}

#[test]
fn file_lease_stale_after_third_party_moves_remote() {
    let fx = Fixture::new();
    fx.seed_commits();
    fx.amend_main();
    let stale = rev_parse(&fx.bare, "refs/heads/main");
    let tree = Command::new("git")
        .current_dir(&fx.bare)
        .args(["rev-parse", "HEAD^{tree}"])
        .output()
        .unwrap();
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();
    let other = Command::new("git")
        .current_dir(&fx.bare)
        .args(["commit-tree", &tree, "-p", &stale.to_hex(), "-m", "other"])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .output()
        .unwrap();
    let other_oid = String::from_utf8_lossy(&other.stdout).trim().to_string();
    git(&fx.bare, &["update-ref", "refs/heads/main", &other_oid]);
    let grit_out = grit_push(
        &fx.grit_client,
        &fx.bare,
        ForceMode::Lease(LeaseExpectation::Tracking),
        &[],
    );
    assert_eq!(grit_out.results[0].status, PushRefStatus::RejectStale);
    assert!(!git_push_lease(&fx.git_client, None));
    assert_eq!(
        remote_tip(&fx.bare),
        ObjectId::from_hex(&other_oid).unwrap()
    );
    fsck_clean(&fx.bare);
}

#[test]
fn file_lease_absent_tracking_rejects_existing_remote() {
    let fx = Fixture::new();
    fx.seed_commits();
    fx.amend_main();
    let _ = std::fs::remove_file(fx.grit_client.join(".git/refs/remotes/origin/main"));
    let _ = std::fs::remove_file(fx.git_client.join(".git/refs/remotes/origin/main"));
    let grit_out = grit_push(
        &fx.grit_client,
        &fx.bare,
        ForceMode::Lease(LeaseExpectation::Tracking),
        &[],
    );
    assert_eq!(grit_out.results[0].status, PushRefStatus::RejectStale);
    assert!(!git_push_lease(&fx.git_client, None));
    fsck_clean(&fx.bare);
}

#[test]
fn file_lease_value_rev_success() {
    let fx = Fixture::new();
    fx.seed_commits();
    fx.amend_main();
    let expected = rev_parse(&fx.bare, "refs/heads/main");
    let grit_ok = grit_push(
        &fx.grit_client,
        &fx.bare,
        ForceMode::Lease(LeaseExpectation::Value(expected)),
        &["main"],
    );
    assert_eq!(grit_ok.results[0].status, PushRefStatus::Ok);
    let lease_arg = format!("--force-with-lease={}", expected.to_hex());
    assert!(git_push_lease(&fx.git_client, Some(lease_arg.as_str())));
    fsck_clean(&fx.bare);
}

#[test]
fn file_lease_value_rev_mismatch() {
    let fx = Fixture::new();
    fx.seed_commits();
    fx.amend_main();
    let wrong = ObjectId::from_hex(&"1".repeat(40)).expect("wrong oid");
    let grit_bad = grit_push(
        &fx.grit_client,
        &fx.bare,
        ForceMode::Lease(LeaseExpectation::Value(wrong)),
        &["main"],
    );
    assert_eq!(grit_bad.results[0].status, PushRefStatus::RejectStale);
    let lease_arg = format!("--force-with-lease={}", wrong.to_hex());
    assert!(!git_push_lease(&fx.git_client, Some(lease_arg.as_str())));
    fsck_clean(&fx.bare);
}

#[test]
fn file_force_overwrites_unconditionally() {
    let fx = Fixture::new();
    fx.seed_commits();
    fx.amend_main();
    let stale = rev_parse(&fx.bare, "refs/heads/main");
    let tree = Command::new("git")
        .current_dir(&fx.bare)
        .args(["rev-parse", "HEAD^{tree}"])
        .output()
        .unwrap();
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();
    let other = Command::new("git")
        .current_dir(&fx.bare)
        .args(["commit-tree", &tree, "-p", &stale.to_hex(), "-m", "other"])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .output()
        .unwrap();
    let other_oid = String::from_utf8_lossy(&other.stdout).trim().to_string();
    git(&fx.bare, &["update-ref", "refs/heads/main", &other_oid]);
    let grit_out = grit_push(&fx.grit_client, &fx.bare, ForceMode::Force, &[]);
    assert_eq!(grit_out.results[0].status, PushRefStatus::Ok);
    assert!(grit_out.results[0].forced);
    assert!(git_push_force(&fx.git_client));
    assert_eq!(remote_tip(&fx.bare), rev_parse(&fx.grit_client, "HEAD"));
    fsck_clean(&fx.bare);
}

#[test]
fn file_lease_delete_stale_when_remote_advanced() {
    let fx = Fixture::new();
    fx.seed_commits();
    let stale = rev_parse(&fx.bare, "refs/heads/main");
    let tree = Command::new("git")
        .current_dir(&fx.bare)
        .args(["rev-parse", "HEAD^{tree}"])
        .output()
        .unwrap();
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();
    let other = Command::new("git")
        .current_dir(&fx.bare)
        .args(["commit-tree", &tree, "-p", &stale.to_hex(), "-m", "other"])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .output()
        .unwrap();
    let other_oid = String::from_utf8_lossy(&other.stdout).trim().to_string();
    git(&fx.bare, &["update-ref", "refs/heads/main", &other_oid]);
    let grit_out = grit_push_request(
        &fx.grit_client,
        &fx.bare,
        PushRequest {
            remote: Some("origin".into()),
            delete: vec!["main".into()],
            force: ForceMode::Lease(LeaseExpectation::Tracking),
            ..Default::default()
        },
    );
    assert_eq!(grit_out.results[0].status, PushRefStatus::RejectStale);
    assert!(!git_push_lease_delete(&fx.git_client));
    assert_eq!(
        remote_tip(&fx.bare),
        ObjectId::from_hex(&other_oid).unwrap()
    );
    fsck_clean(&fx.bare);
}

#[test]
fn file_lease_success_updates_tracking_ref() {
    let fx = Fixture::new();
    fx.seed_commits();
    fx.amend_main();
    let _ = grit_push(
        &fx.grit_client,
        &fx.bare,
        ForceMode::Lease(LeaseExpectation::Tracking),
        &[],
    );
    let tracking = resolve_ref(
        open_repo(&fx.grit_client).git_dir(),
        "refs/remotes/origin/main",
    )
    .expect("tracking");
    assert_eq!(tracking, remote_tip(&fx.bare));
}

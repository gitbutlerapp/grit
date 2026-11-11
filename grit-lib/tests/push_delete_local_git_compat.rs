//! Push ref deletion against a local bare remote (Git-compatible receive-pack).

use std::path::{Path, PathBuf};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::remote::push_plan::{
    open_push_destination, PushPlanError, PushPlanKind, PushRequest,
};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushRefSpec};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

/// A work tree with `main`, `a`, `b` and `c` pushed by `git` to two identical bare remotes,
/// `grit.git` (remote `grit`) and `git.git` (remote `git`).
fn two_remote_fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonical tempdir");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("mkdir work");
    git(&work, &["init", "-q", "-b", "main", "."]);
    std::fs::write(work.join("f"), "1\n").expect("write");
    git(&work, &["add", "f"]);
    git(&work, &["commit", "-q", "-m", "c1"]);
    for name in ["grit", "git"] {
        git(&root, &["init", "-q", "--bare", &format!("{name}.git")]);
        git(&work, &["remote", "add", name, &format!("../{name}.git")]);
        git(
            &work,
            &["push", "-q", name, "main", "main:a", "main:b", "main:c"],
        );
    }
    (tmp, work)
}

fn open_repo(work: &Path) -> Repository {
    let vars = std::env::vars_os().filter(|(k, _)| !k.to_string_lossy().starts_with("GIT_"));
    let env = Environment::from_vars(vars, work.to_path_buf());
    Repository::open(
        &work.join(".git"),
        &RepositoryOptions::with_environment(env).with_work_tree(work),
    )
    .expect("open repo")
}

fn remote_refs(work: &Path, remote: &str) -> String {
    let bare = work.join(format!("../{remote}.git"));
    git(&bare, &["fsck", "--no-progress"]);
    git(
        &bare,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )
}

fn delete_request(remote: &str, names: &[&str]) -> PushRequest {
    PushRequest {
        remote: Some(remote.to_owned()),
        refspecs: names.iter().map(|s| (*s).to_owned()).collect(),
        delete_refs: true,
        ..PushRequest::default()
    }
}

#[test]
fn delete_push_removes_every_named_ref_like_git() {
    let (_tmp, work) = two_remote_fixture();
    let repo = open_repo(&work);
    let config = repo.config().snapshot().expect("config");
    let ctx = repo
        .remotes()
        .push_plan_context(Some("grit"), None)
        .expect("context");
    let plan = repo
        .remotes()
        .resolve_push(&delete_request("grit", &["a", "b"]), &ctx)
        .expect("resolve delete push");
    assert_eq!(plan.kind, PushPlanKind::Delete);
    let outcome = open_push_destination(&config, &plan.remote)
        .expect("destination")
        .push(
            &repo,
            &plan.specs,
            PushOptions::default(),
            &mut NoProgress,
            &CancelToken::never(),
            None,
        )
        .expect("push");
    let statuses: Vec<_> = outcome
        .results
        .iter()
        .map(|r| (r.remote_ref.as_str(), &r.status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("refs/heads/a", &PushRefStatus::Ok),
            ("refs/heads/b", &PushRefStatus::Ok)
        ]
    );

    git(&work, &["push", "-q", "--delete", "git", "a", "b"]);
    let after = remote_refs(&work, "grit");
    assert_eq!(after, remote_refs(&work, "git"));
    assert!(!after.contains("refs/heads/a") && !after.contains("refs/heads/b"));
    assert!(after.contains("refs/heads/c"));
}

#[test]
fn delete_push_with_refspec_leaves_remote_unchanged_like_git() {
    let (_tmp, work) = two_remote_fixture();
    let repo = open_repo(&work);
    let config = repo.config().snapshot().expect("config");
    let before = remote_refs(&work, "grit");
    let ctx = repo
        .remotes()
        .push_plan_context(Some("grit"), None)
        .expect("context");
    let err = repo
        .remotes()
        .resolve_push(
            &delete_request("grit", &["HEAD:refs/heads/release", "b"]),
            &ctx,
        )
        .expect_err("refspec next to --delete");
    assert_eq!(
        err,
        PushPlanError::DeleteNotPlainName {
            spec: "HEAD:refs/heads/release".to_owned()
        }
    );
    assert_eq!(remote_refs(&work, "grit"), before);

    let out = git_cmd(&["push", "--delete", "git", "HEAD:refs/heads/release", "b"])
        .in_dir(&work)
        .exec();
    assert!(!out.ok(), "git accepts a refspec with --delete: {out:?}");
    assert_eq!(remote_refs(&work, "git"), before);
}

#[test]
fn push_delete_tag_on_local_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .current_dir(&origin)
        .status()
        .unwrap();

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&local)
        .status()
        .unwrap();
    std::fs::write(local.join("f"), "x\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "f"])
        .current_dir(&local)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-qm", "c"])
        .current_dir(&local)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["remote", "add", "origin", origin.to_str().unwrap()])
        .current_dir(&local)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["tag", "-a", "v1", "-m", "t"])
        .current_dir(&local)
        .status()
        .unwrap();

    let local_git = local.join(".git");
    let tag_oid = grit_test_support::refs::at(&local_git)
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new("refs/tags/v1").expect("valid ref name"))
        .unwrap();
    grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(&origin),
        &[PushRefSpec::default()
            .with_src(Some(tag_oid))
            .with_dst("refs/tags/v1".to_owned())],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("push tag");
    assert!(grit_test_support::refs::at(&origin)
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new("refs/tags/v1").expect("valid ref name"))
        .is_ok());

    grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(&origin),
        &[PushRefSpec::default()
            .with_dst("refs/tags/v1".to_owned())
            .with_delete(true)],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("delete tag");
    assert!(grit_test_support::refs::at(&origin)
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new("refs/tags/v1").expect("valid ref name"))
        .is_err());
}

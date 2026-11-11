//! Local pushes run the receiving repository's hooks and accept push options, like
//! `git push` to a local path, which goes through `git receive-pack`.
#![cfg(unix)]

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushOutcome, PushRefSpec};
use grit_test_support::{git, git_cmd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Setup {
    _tmp: tempfile::TempDir,
    remote: PathBuf,
    local: PathBuf,
    commit: ObjectId,
}

/// A local repository with one commit on `main`, and an empty bare remote.
fn setup() -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["init", "-q", "-b", "main", "."]);
    git(&local, &["config", "user.name", "a"]);
    git(&local, &["config", "user.email", "a@x"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-qm", "c"]);
    let commit = rev_parse(&local, "HEAD").unwrap();
    Setup {
        _tmp: tmp,
        remote,
        local,
        commit,
    }
}

fn rev_parse(dir: &Path, rev: &str) -> Option<ObjectId> {
    let out = git_cmd(&["rev-parse", "--verify", "-q", rev])
        .in_dir(dir)
        .exec();
    ObjectId::from_hex(out.stdout.trim()).ok()
}

fn write_hook(git_dir: &Path, name: &str, script: &str) {
    let path = git_dir.join("hooks").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn push(
    setup: &Setup,
    refs: &[&str],
    push_options: &[&str],
) -> grit_lib::error::Result<PushOutcome> {
    let specs: Vec<PushRefSpec> = refs
        .iter()
        .map(|dst| {
            PushRefSpec::default()
                .with_src(Some(setup.commit))
                .with_dst((*dst).to_owned())
        })
        .collect();
    let opts = PushOptions::default()
        .with_push_options(push_options.iter().map(|o| (*o).to_owned()).collect());
    grit_lib::repo::Repository::open(
        &setup.local.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(&setup.remote),
        &specs,
        &opts,
        &mut grit_lib::remote::fetch::NoProgress,
        &grit_lib::progress::CancelToken::never(),
    )
}

/// Runs `git push <options> <remote> <refspecs>` and returns whether it succeeded and
/// what it printed.
fn git_push(setup: &Setup, options: &[&str], refspecs: &[&str]) -> (bool, String) {
    let remote = setup.remote.to_str().unwrap();
    let mut argv = vec!["push", "--porcelain"];
    argv.extend(options);
    argv.push(remote);
    argv.extend(refspecs);
    let out = git_cmd(&argv).in_dir(&setup.local).exec();
    (out.status == Some(0), out.stdout + &out.stderr)
}

#[test]
fn update_hook_declines_one_ref() {
    let setup = setup();
    write_hook(
        &setup.remote,
        "update",
        r#"test "$1" != refs/heads/blocked"#,
    );

    let outcome = push(&setup, &["refs/heads/main", "refs/heads/blocked"], &[]).unwrap();
    let main = &outcome.results[0];
    let blocked = &outcome.results[1];
    assert_eq!(main.status, PushRefStatus::Ok);
    assert_eq!(blocked.status, PushRefStatus::RemoteRejected);
    assert_eq!(blocked.message.as_deref(), Some("hook declined"));
    assert!(outcome
        .remote_messages
        .iter()
        .any(|m| m == "error: hook declined to update refs/heads/blocked"));
    assert_eq!(
        rev_parse(&setup.remote, "refs/heads/main"),
        Some(setup.commit)
    );
    assert_eq!(rev_parse(&setup.remote, "refs/heads/blocked"), None);

    // Git reports the same.
    let (ok, text) = git_push(&setup, &[], &["HEAD:refs/heads/blocked"]);
    assert!(!ok);
    assert!(text.contains("[remote rejected] (hook declined)"), "{text}");
}

#[test]
fn pre_receive_hook_declines_every_ref() {
    let setup = setup();
    write_hook(&setup.remote, "pre-receive", "echo nope; exit 1");

    let outcome = push(&setup, &["refs/heads/main", "refs/heads/other"], &[]).unwrap();
    for result in &outcome.results {
        assert_eq!(result.status, PushRefStatus::RemoteRejected);
        assert_eq!(result.message.as_deref(), Some("pre-receive hook declined"));
    }
    assert!(outcome.remote_messages.iter().any(|m| m == "nope"));
    assert_eq!(rev_parse(&setup.remote, "refs/heads/main"), None);

    let (ok, text) = git_push(&setup, &[], &["HEAD:refs/heads/main"]);
    assert!(!ok);
    assert!(
        text.contains("[remote rejected] (pre-receive hook declined)"),
        "{text}"
    );
}

#[test]
fn post_receive_hook_reads_updates_and_its_output_is_relayed() {
    let setup = setup();
    write_hook(
        &setup.remote,
        "post-receive",
        "cat > post-receive.log; echo done",
    );

    let outcome = push(&setup, &["refs/heads/main"], &[]).unwrap();
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert!(outcome.remote_messages.iter().any(|m| m == "done"));
    let zero = ObjectId::null(setup.commit.algo()).to_hex();
    let log = std::fs::read_to_string(setup.remote.join("post-receive.log")).unwrap();
    assert_eq!(log, format!("{zero} {} refs/heads/main\n", setup.commit));
}

#[test]
fn push_options_need_receive_advertise_push_options() {
    let setup = setup();
    write_hook(
        &setup.remote,
        "pre-receive",
        r#"echo "$GIT_PUSH_OPTION_COUNT $GIT_PUSH_OPTION_0" > options.log"#,
    );

    assert!(matches!(
        push(&setup, &["refs/heads/main"], &["ci.skip"]),
        Err(Error::PushOptionsUnsupported)
    ));
    let (ok, _) = git_push(
        &setup,
        &["--push-option=ci.skip"],
        &["HEAD:refs/heads/main"],
    );
    assert!(!ok, "git rejects push options the remote doesn't advertise");

    git(
        &setup.remote,
        &["config", "receive.advertisePushOptions", "true"],
    );
    let outcome = push(&setup, &["refs/heads/main"], &["ci.skip"]).unwrap();
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    let log = std::fs::read_to_string(setup.remote.join("options.log")).unwrap();
    assert_eq!(log, "1 ci.skip\n");
}

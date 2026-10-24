//! The client-side `pre-push` hook during [`Remote::push`] (issue #1799).
//!
//! Hook arguments and stdin are checked against the system `git push` running the same hook
//! script (`git push --dry-run` runs the hook without changing the remote), over a named local
//! remote, an unnamed `file://` URL and SSH.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use grit_lib::command_runner::hooks::{run_pre_push_hook, HookError, PrePushRef};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push_plan::{
    load_push_plan_context, open_push_destination, resolve_push, PushRequest,
};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushOutcome};
use grit_lib::remote::RemoteError;
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    bare: PathBuf,
    log: PathBuf,
}

impl Fixture {
    /// A work tree with one commit on `main`, an empty bare `remote.git` next to it, and
    /// `origin` pointing at `../remote.git`.
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let work = root.join("work");
        let bare = root.join("remote.git");
        std::fs::create_dir_all(&work).expect("mkdir work");
        git(&root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
        git(&work, &["init", "-q", "-b", "main", "."]);
        std::fs::write(work.join("a.txt"), "a\n").expect("write");
        git(&work, &["add", "a.txt"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(&work, &["remote", "add", "origin", "../remote.git"]);
        let log = root.join("hook.log");
        Self {
            _tmp: tmp,
            root,
            work,
            bare,
            log,
        }
    }

    /// Install a `pre-push` hook that appends its arguments and stdin to [`Self::log`].
    fn write_hook(&self, exit: i32) {
        let path = self.work.join(".git/hooks/pre-push");
        std::fs::create_dir_all(path.parent().expect("hooks dir")).expect("mkdir hooks");
        let body = format!(
            "#!/bin/sh\n{{ echo \"args: $1 $2\"; cat; }} >> '{}'\nexit {exit}\n",
            self.log.display()
        );
        std::fs::write(&path, body).expect("write hook");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    fn take_log(&self) -> String {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let _ = std::fs::remove_file(&self.log);
        log
    }

    fn open(&self, extra_env: &[(&str, &str)]) -> Repository {
        let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
            .filter(|(k, _)| !k.to_string_lossy().starts_with("GIT_"))
            .collect();
        vars.extend(extra_env.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        let env = Environment::from_vars(vars, self.work.clone());
        Repository::open(
            &self.work.join(".git"),
            &RepositoryOptions::with_environment(env).with_work_tree(&self.work),
        )
        .expect("open repo")
    }

    fn remote_ref(&self, name: &str) -> Option<String> {
        let out = git_cmd(&["rev-parse", "--verify", "-q", name])
            .in_dir(&self.bare)
            .exec();
        out.ok().then(|| out.stdout.trim().to_owned())
    }

    /// Run the system `git push --dry-run` with the same hook and return what it logged.
    fn git_hook_log(&self, args: &[&str], extra_env: &[(&str, &str)]) -> String {
        let mut cmd = git_cmd(&[&["push", "--dry-run", "-q"], args].concat()).in_dir(&self.work);
        for (k, v) in extra_env {
            cmd = cmd.env(k, v);
        }
        let out = cmd.exec();
        assert!(out.ok(), "git push --dry-run {args:?}: {}", out.stderr);
        self.take_log()
    }
}

/// Plan and push like `grit push <remote> <refspecs>` / `grit push <remote> --delete <refs>`.
fn grit_push(
    repo: &Repository,
    remote: &str,
    refspecs: &[&str],
    delete: &[&str],
    opts: PushOptions,
) -> Result<PushOutcome, RemoteError> {
    let config = repo.config().expect("config");
    let ctx = load_push_plan_context(repo, &config, Some(remote), None).expect("plan context");
    let request = PushRequest {
        remote: Some(remote.to_owned()),
        refspecs: refspecs.iter().map(|s| (*s).to_owned()).collect(),
        delete: delete.iter().map(|s| (*s).to_owned()).collect(),
        ..PushRequest::default()
    };
    let plan = resolve_push(repo, &config, &request, &ctx).expect("resolve push");
    open_push_destination(&config, &plan.remote)?.push(
        repo,
        &plan.specs,
        opts,
        &mut NoProgress,
        None,
    )
}

fn assert_hook_failed(err: RemoteError, expected_status: i32) {
    match err {
        RemoteError::Library(Error::Hook(HookError::Failed { hook_name, status })) => {
            assert_eq!(hook_name, "pre-push");
            assert_eq!(status, expected_status);
        }
        other => panic!("expected a pre-push hook failure, got {other:?}"),
    }
}

fn commit(dir: &Path, file: &str) {
    std::fs::write(dir.join(file), format!("{file}\n")).expect("write");
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", file]);
}

#[test]
fn failing_pre_push_hook_blocks_local_push() {
    let fx = Fixture::new();
    fx.write_hook(1);
    let repo = fx.open(&[]);

    let err = grit_push(&repo, "origin", &["main"], &[], PushOptions::default())
        .expect_err("a failing pre-push hook must stop the push");
    assert_hook_failed(err, 1);
    assert!(fx.take_log().starts_with("args: origin ../remote.git\n"));
    assert_eq!(
        fx.remote_ref("refs/heads/main"),
        None,
        "remote must stay untouched"
    );
    let objects = git(&fx.bare, &["count-objects", "-v"]);
    assert!(objects.contains("count: 0"), "no objects sent: {objects}");
    assert!(
        resolve_ref_opt(&repo, "refs/remotes/origin/main").is_none(),
        "no tracking ref after a blocked push"
    );
}

fn resolve_ref_opt(repo: &Repository, name: &str) -> Option<ObjectId> {
    grit_lib::refs::resolve_ref(repo.git_dir(), name).ok()
}

#[test]
fn failing_pre_push_hook_blocks_dry_run() {
    let fx = Fixture::new();
    fx.write_hook(3);
    let repo = fx.open(&[]);
    let err = grit_push(
        &repo,
        "origin",
        &["main"],
        &[],
        PushOptions {
            dry_run: true,
            ..PushOptions::default()
        },
    )
    .expect_err("dry run still runs the hook");
    assert_hook_failed(err, 3);
}

#[test]
fn no_verify_skips_pre_push_hook() {
    let fx = Fixture::new();
    fx.write_hook(1);
    let repo = fx.open(&[]);
    let outcome = grit_push(
        &repo,
        "origin",
        &["main"],
        &[],
        PushOptions {
            no_verify: true,
            ..PushOptions::default()
        },
    )
    .expect("push with no_verify");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(fx.take_log(), "", "hook must not run");
    let head = git(&fx.work, &["rev-parse", "HEAD"]);
    assert_eq!(
        fx.remote_ref("refs/heads/main").as_deref(),
        Some(head.trim())
    );
    git(&fx.bare, &["fsck", "--strict"]);
}

#[test]
fn pre_push_hook_input_matches_git_over_local_remote() {
    let fx = Fixture::new();
    fx.write_hook(0);
    let repo = fx.open(&[]);

    let cases: &[(&[&str], &[&str], &[&str])] = &[
        (&["main"], &["main"], &[]),
        (&["HEAD:other"], &["HEAD:other"], &[]),
        (&["main"], &["main"], &[]),
        (&[":other"], &[], &["other"]),
    ];
    for (i, (git_args, refspecs, delete)) in cases.iter().enumerate() {
        if i == 2 {
            commit(&fx.work, "b.txt");
        }
        let mut want = fx.git_hook_log(&[&["origin"], *git_args].concat(), &[]);
        if git_args[0].starts_with("HEAD:") {
            // grit's push planner names the branch `HEAD` points at; Git repeats `HEAD`.
            want = want.replace("\nHEAD ", "\nrefs/heads/main ");
        }
        grit_push(&repo, "origin", refspecs, delete, PushOptions::default())
            .unwrap_or_else(|e| panic!("grit push {git_args:?}: {e}"));
        assert_eq!(fx.take_log(), want, "hook input for {git_args:?}");
    }
    assert_eq!(fx.remote_ref("refs/heads/other"), None);
    git(&fx.bare, &["fsck", "--strict"]);
}

#[test]
fn pre_push_hook_runs_when_everything_is_up_to_date() {
    let fx = Fixture::new();
    let repo = fx.open(&[]);
    grit_push(&repo, "origin", &["main"], &[], PushOptions::default()).expect("first push");
    fx.write_hook(0);
    let want = fx.git_hook_log(&["origin", "main"], &[]);
    let outcome =
        grit_push(&repo, "origin", &["main"], &[], PushOptions::default()).expect("second push");
    assert_eq!(outcome.results[0].status, PushRefStatus::UpToDate);
    assert_eq!(fx.take_log(), want);
    assert_eq!(want, "args: origin ../remote.git\n");
}

#[test]
fn pre_push_hook_input_matches_git_over_file_url() {
    let fx = Fixture::new();
    fx.write_hook(0);
    let repo = fx.open(&[]);
    let url = format!("file://{}", fx.bare.display());
    let want = fx.git_hook_log(&[url.as_str(), "main:viaurl"], &[]);
    grit_push(&repo, &url, &["main:viaurl"], &[], PushOptions::default()).expect("file push");
    assert_eq!(fx.take_log(), want);
    assert!(want.starts_with(&format!("args: {url} {url}\n")));
    assert!(fx.remote_ref("refs/heads/viaurl").is_some());
}

fn write_fake_ssh(dir: &Path) -> PathBuf {
    let script = dir.join("fake-ssh.sh");
    let body = r#"#!/bin/sh
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${cmd#git-upload-pack }" ;;
  "git-receive-pack "*) cmd="git receive-pack ${cmd#git-receive-pack }" ;;
esac
eval "exec $cmd"
"#;
    std::fs::write(&script, body).expect("write fake ssh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

#[test]
fn pre_push_hook_gates_ssh_push_like_git() {
    let fx = Fixture::new();
    let ssh = write_fake_ssh(&fx.root);
    let ssh = ssh.to_str().expect("utf8 path");
    let env = [("GIT_SSH_COMMAND", ssh)];
    let url = format!("git@localhost:{}", fx.bare.display());
    git(&fx.work, &["remote", "add", "ssh", &url]);
    let repo = fx.open(&env);

    fx.write_hook(0);
    let want = fx.git_hook_log(&["ssh", "main"], &env);
    grit_push(&repo, "ssh", &["main"], &[], PushOptions::default()).expect("ssh push");
    assert_eq!(fx.take_log(), want);
    assert!(want.starts_with(&format!("args: ssh {url}\n")));

    commit(&fx.work, "c.txt");
    fx.write_hook(7);
    let before = fx.remote_ref("refs/heads/main");
    let err = grit_push(&repo, "ssh", &["main"], &[], PushOptions::default())
        .expect_err("failing hook blocks the ssh push");
    assert_hook_failed(err, 7);
    assert_eq!(fx.remote_ref("refs/heads/main"), before);
    git(&fx.bare, &["fsck", "--strict"]);
}

#[test]
fn run_pre_push_hook_formats_stdin_lines() {
    let fx = Fixture::new();
    fx.write_hook(0);
    let repo = fx.open(&[]);
    let a = ObjectId::from_hex(&"a".repeat(40)).expect("oid");
    let b = ObjectId::from_hex(&"b".repeat(40)).expect("oid");
    let zero = "0".repeat(40);
    run_pre_push_hook(
        &repo,
        "up",
        "https://example.com/r.git",
        &[
            PrePushRef {
                local_ref: Some("refs/heads/main".to_owned()),
                local_oid: Some(a),
                remote_ref: "refs/heads/main".to_owned(),
                remote_oid: Some(b),
            },
            PrePushRef {
                local_ref: None,
                local_oid: Some(a),
                remote_ref: "refs/heads/new".to_owned(),
                remote_oid: None,
            },
            PrePushRef {
                local_ref: None,
                local_oid: None,
                remote_ref: "refs/heads/gone".to_owned(),
                remote_oid: Some(b),
            },
        ],
    )
    .expect("hook succeeds");
    let a = a.to_hex();
    let b = b.to_hex();
    assert_eq!(
        fx.take_log(),
        format!(
            "args: up https://example.com/r.git\n\
             refs/heads/main {a} refs/heads/main {b}\n\
             {a} {a} refs/heads/new {zero}\n\
             (delete) {zero} refs/heads/gone {b}\n"
        )
    );

    fx.write_hook(4);
    let err = run_pre_push_hook(&repo, "up", "u", &[]).expect_err("failing hook");
    assert!(
        matches!(err, HookError::Failed { status: 4, .. }),
        "{err:?}"
    );
}

#[test]
fn run_pre_push_hook_without_a_hook_is_ok() {
    let fx = Fixture::new();
    let repo = fx.open(&[]);
    run_pre_push_hook(&repo, "origin", "../remote.git", &[]).expect("no hook installed");
}

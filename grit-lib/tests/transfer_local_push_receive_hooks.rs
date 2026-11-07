//! Receive-side hooks during a local-path or `file://` push (issue #2171).
//!
//! The remote's `pre-receive`, `update`, `post-receive` and `post-update` hooks must run as
//! they do when the system `git push` writes to the same repository: a declining hook keeps
//! the ref and the pushed objects out, hook output reaches the pusher, and push options reach
//! the hooks when the remote sets `receive.advertisePushOptions`. Every scenario is
//! cross-checked against `git push` on an identical fixture, and `git fsck` checks the result.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::push_plan::{open_push_destination, PushRequest};
use grit_lib::remote::push_report::PushRefStatus;
use grit_lib::remote::transfer::{PushOptions, PushOutcome, PushRefSpec};
use grit_lib::remote::RemoteError;
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

/// Hook that records its name, push options and stdin, then checks that every pushed tip is
/// readable, which needs the quarantine variables while `pre-receive` runs.
const LOGGING_RECEIVE_HOOK: &str = r#"#!/bin/sh
input=$(cat)
{
  echo "$HOOK_NAME count=${GIT_PUSH_OPTION_COUNT:-0} opt0=${GIT_PUSH_OPTION_0:-}"
  echo "$input"
} >> "$LOG"
echo "$input" | while read old new ref; do
  case "$new" in
    *[!0]*) git cat-file -e "$new" || exit 3 ;;
  esac
done || exit 3
"#;

struct Fixture {
    _tmp: tempfile::TempDir,
    work: PathBuf,
    /// The bare repository grit pushes to.
    bare: PathBuf,
    /// An identical bare repository the system `git push` writes to.
    git_bare: PathBuf,
}

impl Fixture {
    /// A work tree with one commit on `main` and a branch `feature`, plus two empty bare
    /// repositories next to it: `remote.git` (remote `origin`) and `git-remote.git`
    /// (remote `gitside`).
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical tempdir");
        let work = root.join("work");
        std::fs::create_dir_all(&work).expect("mkdir work");
        git(&root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
        git(
            &root,
            &["init", "-q", "--bare", "-b", "main", "git-remote.git"],
        );
        git(&work, &["init", "-q", "-b", "main", "."]);
        std::fs::write(work.join("f"), "1\n").expect("write");
        git(&work, &["add", "f"]);
        git(&work, &["commit", "-q", "-m", "one"]);
        git(&work, &["branch", "feature"]);
        git(&work, &["remote", "add", "origin", "../remote.git"]);
        git(&work, &["remote", "add", "gitside", "../git-remote.git"]);
        Self {
            bare: root.join("remote.git"),
            git_bare: root.join("git-remote.git"),
            _tmp: tmp,
            work,
        }
    }

    /// Install `body` as hook `name` in both bare repositories. `$LOG` in the script is the
    /// repository's own log file (see [`Self::log`]) and `$HOOK_NAME` is `name`.
    fn hook(&self, name: &str, body: &str) {
        for bare in [&self.bare, &self.git_bare] {
            let log = Self::log_path(bare);
            let script = body.replacen(
                "#!/bin/sh\n",
                &format!("#!/bin/sh\nLOG='{}'\nHOOK_NAME={name}\n", log.display()),
                1,
            );
            let path = bare.join("hooks").join(name);
            std::fs::create_dir_all(path.parent().expect("hooks dir")).expect("mkdir hooks");
            std::fs::write(&path, script).expect("write hook");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    fn config(&self, key: &str, value: &str) {
        for bare in [&self.bare, &self.git_bare] {
            git(bare, &["config", key, value]);
        }
    }

    fn log_path(bare: &Path) -> PathBuf {
        bare.with_extension("log")
    }

    /// What the hooks of `bare` logged so far; empty when no hook wrote anything.
    fn log(bare: &Path) -> String {
        std::fs::read_to_string(Self::log_path(bare)).unwrap_or_default()
    }

    fn open(&self) -> Repository {
        let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
            .filter(|(k, _)| !k.to_string_lossy().starts_with("GIT_"))
            .collect();
        let env = Environment::from_vars(vars, self.work.clone());
        Repository::open(
            &self.work.join(".git"),
            &RepositoryOptions::with_environment(env).with_work_tree(&self.work),
        )
        .expect("open repo")
    }

    fn remote_ref(bare: &Path, name: &str) -> Option<String> {
        let out = git_cmd(&["rev-parse", "--verify", "-q", name])
            .in_dir(bare)
            .exec();
        out.ok().then(|| out.stdout.trim().to_owned())
    }

    /// `"origin"` stays the remote name; `"file"` becomes the `file://` URL of `remote.git`.
    fn destination(&self, kind: &str) -> String {
        match kind {
            "file" => format!("file://{}", self.bare.display()),
            other => other.to_owned(),
        }
    }

    fn head(&self) -> String {
        git(&self.work, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// `git push <args>` from the work tree to `gitside`; returns whether it succeeded and
    /// its stderr.
    fn git_push(&self, args: &[&str]) -> (bool, String) {
        let out = git_cmd(&[&["push", "-q"], args].concat())
            .in_dir(&self.work)
            .exec();
        (out.ok(), out.stderr)
    }

    /// Objects stored in `bare`, loose and packed.
    fn object_count(bare: &Path) -> usize {
        let out = git(bare, &["count-objects", "-v"]);
        out.lines()
            .filter_map(|l| {
                l.strip_prefix("count: ")
                    .or_else(|| l.strip_prefix("in-pack: "))
            })
            .map(|n| n.trim().parse::<usize>().expect("count"))
            .sum()
    }

    /// Leftover quarantine directories under `bare/objects`.
    fn quarantine_dirs(bare: &Path) -> Vec<String> {
        std::fs::read_dir(bare.join("objects"))
            .expect("objects dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("tmp_objdir"))
            .collect()
    }
}

/// Plan and push like `grit push <remote> <refspecs>`.
fn grit_push(
    repo: &Repository,
    remote: &str,
    refspecs: &[&str],
    opts: PushOptions,
) -> grit_lib::error::Result<PushOutcome> {
    let config = repo.config().snapshot().expect("config");
    let ctx = repo
        .remotes()
        .push_plan_context(Some(remote), None)
        .expect("plan context");
    let request = PushRequest {
        remote: Some(remote.to_owned()),
        refspecs: refspecs.iter().map(|s| (*s).to_owned()).collect(),
        ..PushRequest::default()
    };
    let plan = repo
        .remotes()
        .resolve_push(&request, &ctx)
        .expect("resolve push");
    open_push_destination(&config, &plan.remote)?.push(
        repo,
        &plan.specs,
        opts,
        &mut NoProgress,
        &CancelToken::never(),
        None,
    )
}

/// The push destinations from the issue: `origin` (the relative path `../remote.git`) and a
/// `file://` URL (see [`Fixture::destination`]).
const DESTINATIONS: [&str; 2] = ["origin", "file"];

#[test]
fn declining_pre_receive_keeps_ref_and_objects_out() {
    for dest in DESTINATIONS {
        let fx = Fixture::new();
        let dest = fx.destination(dest);
        fx.hook(
            "pre-receive",
            "#!/bin/sh\ncat >/dev/null\necho \"policy: no\" >&2\nexit 1\n",
        );
        fx.hook("post-receive", "#!/bin/sh\necho posted >> \"$LOG\"\n");
        fx.hook("post-update", "#!/bin/sh\necho post-update >> \"$LOG\"\n");
        let repo = fx.open();

        let outcome = grit_push(&repo, &dest, &["main"], PushOptions::default())
            .unwrap_or_else(|e| panic!("push to {dest}: {e}"));
        assert_eq!(outcome.results.len(), 1, "{dest}");
        assert_eq!(
            outcome.results[0].status,
            PushRefStatus::RemoteRejected,
            "{dest}"
        );
        assert_eq!(
            outcome.results[0].message.as_deref(),
            Some("pre-receive hook declined"),
            "{dest}"
        );
        assert_eq!(outcome.remote_messages, ["policy: no"], "{dest}");
        assert_eq!(Fixture::remote_ref(&fx.bare, "refs/heads/main"), None);
        assert_eq!(Fixture::log(&fx.bare), "", "no post-* hook after a decline");
        assert_eq!(Fixture::object_count(&fx.bare), 0, "objects stay out");
        assert!(Fixture::quarantine_dirs(&fx.bare).is_empty());
        assert!(
            repo.references()
                .resolve("refs/remotes/origin/main")
                .is_err(),
            "no tracking ref after a declined push"
        );
        git(&fx.bare, &["fsck", "--strict"]);

        let (ok, stderr) = fx.git_push(&["gitside", "main"]);
        assert!(!ok, "git push must be declined too");
        assert!(stderr.contains("policy: no"), "{stderr}");
        assert_eq!(Fixture::remote_ref(&fx.git_bare, "refs/heads/main"), None);
        assert_eq!(Fixture::log(&fx.git_bare), "");
    }
}

#[test]
fn successful_push_runs_every_receive_hook_like_git() {
    for dest in DESTINATIONS {
        let fx = Fixture::new();
        let dest = fx.destination(dest);
        fx.hook("pre-receive", LOGGING_RECEIVE_HOOK);
        fx.hook("post-receive", LOGGING_RECEIVE_HOOK);
        fx.hook(
            "update",
            "#!/bin/sh\necho \"update $1 $2 $3\" >> \"$LOG\"\necho \"checked $1\"\n",
        );
        fx.hook(
            "post-update",
            "#!/bin/sh\necho \"post-update $*\" >> \"$LOG\"\n",
        );
        let repo = fx.open();

        let outcome = grit_push(&repo, &dest, &["main", "feature"], PushOptions::default())
            .unwrap_or_else(|e| panic!("push to {dest}: {e}"));
        assert!(
            outcome
                .results
                .iter()
                .all(|r| r.status == PushRefStatus::Ok),
            "{dest}: {:?}",
            outcome.results
        );
        assert_eq!(
            outcome.remote_messages,
            ["checked refs/heads/main", "checked refs/heads/feature"],
            "{dest}"
        );
        let head = fx.head();
        assert_eq!(
            Fixture::remote_ref(&fx.bare, "refs/heads/main").as_deref(),
            Some(head.as_str())
        );
        git(&fx.bare, &["fsck", "--strict"]);
        assert!(Fixture::quarantine_dirs(&fx.bare).is_empty());

        let (ok, stderr) = fx.git_push(&["gitside", "main", "feature"]);
        assert!(ok, "git push: {stderr}");
        assert_eq!(
            Fixture::log(&fx.bare),
            Fixture::log(&fx.git_bare),
            "{dest}: hook arguments and stdin must match git"
        );
        assert!(Fixture::log(&fx.bare)
            .contains(&format!("post-update refs/heads/main refs/heads/feature\n")));
    }
}

#[test]
fn push_options_reach_receive_hooks_when_advertised() {
    for dest in DESTINATIONS {
        let fx = Fixture::new();
        let dest = fx.destination(dest);
        fx.config("receive.advertisePushOptions", "true");
        fx.hook("pre-receive", LOGGING_RECEIVE_HOOK);
        fx.hook("post-receive", LOGGING_RECEIVE_HOOK);
        let repo = fx.open();

        let outcome = grit_push(
            &repo,
            &dest,
            &["main"],
            PushOptions {
                push_options: vec!["ci=1".to_owned()],
                ..PushOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("push to {dest}: {e}"));
        assert_eq!(outcome.results[0].status, PushRefStatus::Ok, "{dest}");
        let log = Fixture::log(&fx.bare);
        assert!(log.starts_with("pre-receive count=1 opt0=ci=1\n"), "{log}");
        assert!(log.contains("post-receive count=1 opt0=ci=1\n"), "{log}");

        let (ok, stderr) = fx.git_push(&["-o", "ci=1", "gitside", "main"]);
        assert!(ok, "git push -o: {stderr}");
        assert_eq!(log, Fixture::log(&fx.git_bare), "{dest}");
        git(&fx.bare, &["fsck", "--strict"]);
    }
}

#[test]
fn push_options_fail_before_writing_when_not_advertised() {
    for setting in [None, Some("false")] {
        let fx = Fixture::new();
        if let Some(value) = setting {
            fx.config("receive.advertisePushOptions", value);
        }
        fx.hook("pre-receive", LOGGING_RECEIVE_HOOK);
        let repo = fx.open();
        let opts = PushOptions {
            push_options: vec!["ci=1".to_owned()],
            ..PushOptions::default()
        };
        for dry_run in [false, true] {
            let err = grit_push(
                &repo,
                "origin",
                &["main"],
                PushOptions {
                    dry_run,
                    ..opts.clone()
                },
            )
            .expect_err("push options need receive.advertisePushOptions");
            assert!(
                matches!(err, RemoteError::PushOptionsUnsupported),
                "{setting:?}: {err:?}"
            );
        }
        assert_eq!(Fixture::remote_ref(&fx.bare, "refs/heads/main"), None);
        assert_eq!(Fixture::object_count(&fx.bare), 0);
        assert_eq!(Fixture::log(&fx.bare), "", "no hook runs");

        let (ok, _) = fx.git_push(&["-o", "ci=1", "gitside", "main"]);
        assert!(!ok, "git push -o must fail too ({setting:?})");
        assert_eq!(Fixture::remote_ref(&fx.git_bare, "refs/heads/main"), None);
    }
}

#[test]
fn update_hook_declines_only_its_ref() {
    let fx = Fixture::new();
    fx.hook(
        "update",
        "#!/bin/sh\ncase \"$1\" in refs/heads/feature) echo \"no $1\" >&2; exit 1;; esac\n",
    );
    fx.hook("post-receive", LOGGING_RECEIVE_HOOK);
    let repo = fx.open();

    let outcome = grit_push(
        &repo,
        "origin",
        &["main", "feature"],
        PushOptions::default(),
    )
    .expect("push");
    let status = |name: &str| {
        outcome
            .results
            .iter()
            .find(|r| r.remote_ref == name)
            .map(|r| (r.status.clone(), r.message.clone()))
            .expect("result")
    };
    assert_eq!(status("refs/heads/main").0, PushRefStatus::Ok);
    assert_eq!(
        status("refs/heads/feature"),
        (
            PushRefStatus::RemoteRejected,
            Some("hook declined".to_owned())
        )
    );
    assert_eq!(
        outcome.remote_messages,
        [
            "no refs/heads/feature",
            "error: hook declined to update refs/heads/feature"
        ]
    );
    let head = fx.head();
    assert_eq!(
        Fixture::remote_ref(&fx.bare, "refs/heads/main").as_deref(),
        Some(head.as_str())
    );
    assert_eq!(Fixture::remote_ref(&fx.bare, "refs/heads/feature"), None);
    git(&fx.bare, &["fsck", "--strict"]);

    let (ok, stderr) = fx.git_push(&["gitside", "main", "feature"]);
    assert!(!ok, "git reports the declined ref");
    assert!(
        stderr.contains("remote: error: hook declined to update refs/heads/feature"),
        "{stderr}"
    );
    assert_eq!(
        Fixture::log(&fx.bare),
        Fixture::log(&fx.git_bare),
        "post-receive sees only the accepted update"
    );
    assert_eq!(
        Fixture::remote_ref(&fx.git_bare, "refs/heads/feature"),
        None
    );
}

#[test]
fn atomic_push_declined_by_update_hook_writes_nothing() {
    let fx = Fixture::new();
    fx.hook(
        "update",
        "#!/bin/sh\ncase \"$1\" in refs/heads/feature) exit 1;; esac\n",
    );
    fx.hook("post-receive", LOGGING_RECEIVE_HOOK);
    let repo = fx.open();

    let outcome = grit_push(
        &repo,
        "origin",
        &["main", "feature"],
        PushOptions {
            atomic: true,
            ..PushOptions::default()
        },
    )
    .expect("push");
    let statuses: Vec<_> = outcome
        .results
        .iter()
        .map(|r| (r.remote_ref.as_str(), r.status.clone()))
        .collect();
    assert_eq!(
        statuses,
        [
            ("refs/heads/main", PushRefStatus::AtomicPushFailed),
            ("refs/heads/feature", PushRefStatus::RemoteRejected),
        ]
    );
    assert_eq!(Fixture::remote_ref(&fx.bare, "refs/heads/main"), None);
    assert_eq!(Fixture::remote_ref(&fx.bare, "refs/heads/feature"), None);
    assert_eq!(Fixture::log(&fx.bare), "", "post-receive does not run");

    let (ok, _) = fx.git_push(&["--atomic", "gitside", "main", "feature"]);
    assert!(!ok);
    assert_eq!(Fixture::remote_ref(&fx.git_bare, "refs/heads/main"), None);
    assert_eq!(Fixture::log(&fx.git_bare), "");
}

#[test]
fn atomic_push_declined_by_pre_receive_rejects_every_ref() {
    let fx = Fixture::new();
    fx.hook("pre-receive", "#!/bin/sh\ncat >/dev/null\nexit 1\n");
    let repo = fx.open();
    let outcome = grit_push(
        &repo,
        "origin",
        &["main", "feature"],
        PushOptions {
            atomic: true,
            ..PushOptions::default()
        },
    )
    .expect("push");
    assert!(outcome.results.iter().all(|r| {
        r.status == PushRefStatus::RemoteRejected
            && r.message.as_deref() == Some("pre-receive hook declined")
    }));
    assert_eq!(Fixture::remote_ref(&fx.bare, "refs/heads/main"), None);
    assert_eq!(Fixture::object_count(&fx.bare), 0);
}

#[test]
fn dry_run_runs_no_receive_hook() {
    let fx = Fixture::new();
    fx.config("receive.advertisePushOptions", "true");
    for name in ["pre-receive", "update", "post-receive", "post-update"] {
        fx.hook(name, LOGGING_RECEIVE_HOOK);
    }
    let repo = fx.open();
    let outcome = grit_push(
        &repo,
        "origin",
        &["main"],
        PushOptions {
            dry_run: true,
            push_options: vec!["ci=1".to_owned()],
            ..PushOptions::default()
        },
    )
    .expect("dry run");
    assert_eq!(outcome.results[0].status, PushRefStatus::Ok);
    assert_eq!(Fixture::log(&fx.bare), "");
    assert_eq!(Fixture::remote_ref(&fx.bare, "refs/heads/main"), None);

    let (ok, stderr) = fx.git_push(&["--dry-run", "-o", "ci=1", "gitside", "main"]);
    assert!(ok, "{stderr}");
    assert_eq!(Fixture::log(&fx.git_bare), "", "git runs no hook either");
}

#[test]
fn push_local_runs_receive_hooks() {
    let fx = Fixture::new();
    fx.hook(
        "pre-receive",
        "#!/bin/sh\nwhile read old new ref; do echo \"deny $ref\" >&2; done\nexit 1\n",
    );
    let head = grit_lib::objects::ObjectId::from_hex(&fx.head()).expect("oid");
    let outcome = grit_lib::repo::Repository::open(
        &fx.work.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open push source")
    .remotes()
    .push_to(
        grit_lib::remote::transfer::TransferEndpoint::Local(&fx.bare),
        &[PushRefSpec {
            src: Some(head),
            src_ref: Some("refs/heads/main".to_owned()),
            dst: "refs/heads/main".to_owned(),
            ..PushRefSpec::default()
        }],
        &PushOptions::default(),
        &mut grit_lib::remote::fetch::NoProgress,
        &CancelToken::never(),
    )
    .expect("push_local");
    assert_eq!(outcome.results[0].status, PushRefStatus::RemoteRejected);
    assert_eq!(outcome.remote_messages, ["deny refs/heads/main"]);
    assert_eq!(Fixture::remote_ref(&fx.bare, "refs/heads/main"), None);
}

//! Fetch, push, prune, `set-head` and inspect on a [`Repository::with_ref_store`] handle keep
//! every remote-tracking ref in the injected store and write nothing under `<git_dir>/refs`
//! (issue #2128). The same operations on a default files repository stay readable by the
//! system `git`.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::progress::NeverCancel;
use grit_lib::refs::store::{MemoryRefStore, RawRef, RefStore};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec};
use grit_lib::remote::InspectOptions;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::SetHead;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn environment() -> Environment {
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    env
}

/// An upstream work tree with `main` and `topic`, and an empty bare push target.
struct Remotes {
    upstream: std::path::PathBuf,
    bare: std::path::PathBuf,
    tip: ObjectId,
}

fn remotes(root: &Path) -> Remotes {
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream).expect("mkdir");
    git(&upstream, &["init", "-q", "-b", "main"]);
    git(&upstream, &["config", "uploadpack.allowFilter", "true"]);
    fs::write(upstream.join("a"), "a\n").expect("write");
    git(&upstream, &["add", "."]);
    git(&upstream, &["commit", "-q", "-m", "one"]);
    git(&upstream, &["branch", "topic"]);
    let bare = root.join("bare.git");
    git(
        root,
        &["init", "-q", "--bare", bare.to_str().expect("utf8")],
    );
    let tip = git(&upstream, &["rev-parse", "HEAD"]).parse().expect("oid");
    Remotes {
        upstream,
        bare,
        tip,
    }
}

/// A local files repository with remotes `origin` (upstream) and `pub` (bare).
fn local_repo(root: &Path, remotes: &Remotes) -> std::path::PathBuf {
    let local = root.join("local");
    Repository::init(&local, &InitOptions::new().with_initial_branch("main")).expect("init");
    let config = local.join(".git/config");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push_str(&format!(
        "[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\
         [remote \"pub\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/pub/*\n",
        remotes.upstream.display(),
        remotes.bare.display()
    ));
    fs::write(&config, text).expect("write config");
    local
}

fn open(local: &Path) -> Repository {
    Repository::open(
        &local.join(".git"),
        &RepositoryOptions::with_environment(environment()).with_work_tree(local),
    )
    .expect("open")
}

fn store_refs(store: &MemoryRefStore, prefix: &str) -> Vec<(String, RawRef)> {
    let mut out = Vec::new();
    store
        .for_each_ref(prefix, &mut |entry| {
            out.push((entry.name.clone(), entry.value.clone()));
            ControlFlow::Continue(())
        })
        .expect("list");
    out
}

fn fetch(repo: &Repository, remote: &str, opts: FetchOptions) {
    repo.remotes()
        .get(remote)
        .expect("remote")
        .fetch(repo, opts, &mut NoProgress, &NeverCancel, None)
        .expect("fetch");
}

/// Nothing a fetch or push did may surface as on-disk refs of the local repository.
fn assert_no_disk_tracking_refs(local: &Path) {
    let git_dir = local.join(".git");
    assert!(
        !git_dir.join("refs/remotes").exists(),
        "tracking refs were written under <git_dir>/refs/remotes"
    );
    let packed = fs::read_to_string(git_dir.join("packed-refs")).unwrap_or_default();
    assert!(
        !packed.contains("refs/remotes/"),
        "tracking refs were packed on disk: {packed}"
    );
    assert_eq!(
        git(
            local,
            &["for-each-ref", "--format=%(refname)", "refs/remotes/"]
        ),
        ""
    );
}

fn assert_origin_tracking_in_store(repo: &Repository, store: &MemoryRefStore, tip: ObjectId) {
    assert_eq!(
        store_refs(store, "refs/remotes/origin/"),
        vec![
            (
                "refs/remotes/origin/HEAD".to_owned(),
                RawRef::Symbolic("refs/remotes/origin/main".to_owned())
            ),
            ("refs/remotes/origin/main".to_owned(), RawRef::Direct(tip)),
            ("refs/remotes/origin/topic".to_owned(), RawRef::Direct(tip)),
        ]
    );
    assert_eq!(
        repo.rev_walk()
            .resolve("refs/remotes/origin/main")
            .expect("resolve through handle"),
        tip
    );
    assert_eq!(
        repo.rev_walk()
            .resolve("refs/remotes/origin/HEAD")
            .expect("resolve HEAD"),
        tip
    );
}

#[test]
fn local_fetch_and_prune_stay_in_the_injected_store() {
    let tmp = tempfile::tempdir().expect("tmp");
    let remotes = remotes(tmp.path());
    let local = local_repo(tmp.path(), &remotes);
    let store = Arc::new(MemoryRefStore::new());
    let repo = open(&local).with_ref_store(store.clone());

    fetch(&repo, "origin", FetchOptions::default());
    assert_origin_tracking_in_store(&repo, &store, remotes.tip);
    assert_no_disk_tracking_refs(&local);

    git(&remotes.upstream, &["branch", "-D", "topic"]);
    store
        .set_ref("refs/remotes/origin/gone", RawRef::Direct(remotes.tip))
        .expect("seed stale ref");
    fetch(&repo, "origin", {
        let mut o = FetchOptions::default();
        o.prune = true;
        o
    });
    let names: Vec<String> = store_refs(&store, "refs/remotes/origin/")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        ["refs/remotes/origin/HEAD", "refs/remotes/origin/main"]
    );
    assert_no_disk_tracking_refs(&local);
}

#[test]
fn wire_fetch_stays_in_the_injected_store() {
    let tmp = tempfile::tempdir().expect("tmp");
    let remotes = remotes(tmp.path());
    let local = local_repo(tmp.path(), &remotes);
    let store = Arc::new(MemoryRefStore::new());
    let repo = open(&local).with_ref_store(store.clone());

    // A partial-clone filter sends a local fetch through `git upload-pack` and the wire path.
    fetch(&repo, "origin", {
        let mut o = FetchOptions::default();
        o.filter = Some(ObjectFilterSpec::BlobNone);
        o
    });
    assert_origin_tracking_in_store(&repo, &store, remotes.tip);
    assert_no_disk_tracking_refs(&local);

    git(&remotes.upstream, &["branch", "-D", "topic"]);
    fetch(&repo, "origin", {
        let mut o = FetchOptions::default();
        o.filter = Some(ObjectFilterSpec::BlobNone);
        o.prune = true;
        o
    });
    assert!(store
        .read_raw("refs/remotes/origin/topic")
        .expect("read")
        .is_none());
    assert_no_disk_tracking_refs(&local);
}

#[test]
fn bundle_fetch_stays_in_the_injected_store() {
    let tmp = tempfile::tempdir().expect("tmp");
    let remotes = remotes(tmp.path());
    let bundle = tmp.path().join("all.bundle");
    git(
        &remotes.upstream,
        &[
            "bundle",
            "create",
            "-q",
            bundle.to_str().expect("utf8"),
            "--all",
        ],
    );
    let local = local_repo(tmp.path(), &remotes);
    git(
        &local,
        &[
            "remote",
            "set-url",
            "origin",
            bundle.to_str().expect("utf8"),
        ],
    );
    let store = Arc::new(MemoryRefStore::new());
    store
        .set_ref("refs/remotes/origin/gone", RawRef::Direct(remotes.tip))
        .expect("seed stale ref");
    let repo = open(&local).with_ref_store(store.clone());

    fetch(&repo, "origin", {
        let mut o = FetchOptions::default();
        o.prune = true;
        o
    });
    assert_eq!(
        store_refs(&store, "refs/remotes/origin/"),
        vec![
            (
                "refs/remotes/origin/main".to_owned(),
                RawRef::Direct(remotes.tip)
            ),
            (
                "refs/remotes/origin/topic".to_owned(),
                RawRef::Direct(remotes.tip)
            ),
        ]
    );
    assert_no_disk_tracking_refs(&local);
}

#[test]
fn push_updates_tracking_refs_in_the_injected_store() {
    let tmp = tempfile::tempdir().expect("tmp");
    let remotes = remotes(tmp.path());
    let local = local_repo(tmp.path(), &remotes);
    let store = Arc::new(MemoryRefStore::new());
    let repo = open(&local).with_ref_store(store.clone());
    fetch(&repo, "origin", FetchOptions::default());
    store
        .set_ref("refs/heads/main", RawRef::Direct(remotes.tip))
        .expect("branch");

    let spec = PushRefSpec {
        src: Some(remotes.tip),
        src_ref: Some("refs/heads/main".into()),
        dst: "refs/heads/main".into(),
        ..Default::default()
    };
    repo.remotes()
        .get("pub")
        .expect("remote")
        .push(
            &repo,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("push");

    assert_eq!(
        store_refs(&store, "refs/remotes/pub/"),
        vec![(
            "refs/remotes/pub/main".to_owned(),
            RawRef::Direct(remotes.tip)
        )]
    );
    assert_no_disk_tracking_refs(&local);
    assert_eq!(
        git(&remotes.bare, &["rev-parse", "refs/heads/main"]),
        remotes.tip.to_hex()
    );
    git(&remotes.bare, &["fsck", "--strict"]);
}

#[test]
fn remote_prune_set_head_and_inspect_use_the_injected_store() {
    let tmp = tempfile::tempdir().expect("tmp");
    let remotes = remotes(tmp.path());
    let local = local_repo(tmp.path(), &remotes);
    let store = Arc::new(MemoryRefStore::new());
    let repo = open(&local).with_ref_store(store.clone());
    fetch(&repo, "origin", FetchOptions::default());
    git(&remotes.upstream, &["branch", "-D", "topic"]);

    let summary = repo
        .remotes()
        .inspect("origin", &InspectOptions::default(), None)
        .expect("inspect");
    let states: Vec<(String, String)> = summary
        .branches
        .iter()
        .map(|b| (b.name.clone(), format!("{:?}", b.state)))
        .collect();
    assert_eq!(
        states,
        [
            ("main".to_owned(), "Tracked".to_owned()),
            ("topic".to_owned(), "Stale".to_owned())
        ]
    );

    let offline = repo
        .remotes()
        .inspect(
            "origin",
            &InspectOptions {
                query: false,
                limit: None,
            },
            None,
        )
        .expect("inspect offline");
    assert_eq!(offline.head_branch.as_deref(), Some("main"));
    assert_eq!(offline.branches.len(), 2);

    let pruned = repo
        .remotes()
        .prune("origin", false, None)
        .expect("remote prune");
    assert_eq!(pruned, ["refs/remotes/origin/topic"]);
    assert!(store
        .read_raw("refs/remotes/origin/topic")
        .expect("read")
        .is_none());

    let deleted = repo
        .remotes()
        .set_head("origin", SetHead::Delete, false, None)
        .expect("delete head");
    assert_eq!(
        deleted.previous_target.as_deref(),
        Some("refs/remotes/origin/main")
    );
    assert!(store
        .read_raw("refs/remotes/origin/HEAD")
        .expect("read")
        .is_none());

    let set = repo
        .remotes()
        .set_head("origin", SetHead::Branch("main".into()), false, None)
        .expect("set head");
    assert_eq!(set.previous_target, None);
    assert_eq!(
        store.read_raw("refs/remotes/origin/HEAD").expect("read"),
        Some(RawRef::Symbolic("refs/remotes/origin/main".into()))
    );
    assert!(repo
        .remotes()
        .set_head("origin", SetHead::Branch("topic".into()), false, None)
        .is_err());
    assert_no_disk_tracking_refs(&local);
}

#[test]
fn files_backend_fetch_prune_and_push_round_trip_with_git() {
    let tmp = tempfile::tempdir().expect("tmp");
    let remotes = remotes(tmp.path());
    let local = local_repo(tmp.path(), &remotes);
    let repo = open(&local);

    fetch(&repo, "origin", FetchOptions::default());
    git(&remotes.upstream, &["branch", "-D", "topic"]);
    fetch(&repo, "origin", {
        let mut o = FetchOptions::default();
        o.prune = true;
        o
    });
    git(
        &local,
        &["update-ref", "refs/heads/main", &remotes.tip.to_hex()],
    );
    let spec = PushRefSpec {
        src: Some(remotes.tip),
        src_ref: Some("refs/heads/main".into()),
        dst: "refs/heads/main".into(),
        ..Default::default()
    };
    repo.remotes()
        .get("pub")
        .expect("remote")
        .push(
            &repo,
            &[spec],
            PushOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("push");

    assert_eq!(
        git(
            &local,
            &["for-each-ref", "--format=%(refname) %(objectname)", "refs/remotes/"]
        ),
        format!(
            "refs/remotes/origin/HEAD {tip}\nrefs/remotes/origin/main {tip}\nrefs/remotes/pub/main {tip}",
            tip = remotes.tip.to_hex()
        )
    );
    assert_eq!(
        git(&local, &["symbolic-ref", "refs/remotes/origin/HEAD"]),
        "refs/remotes/origin/main"
    );
    git(&local, &["fsck", "--strict"]);
    git(&remotes.bare, &["fsck", "--strict"]);
}

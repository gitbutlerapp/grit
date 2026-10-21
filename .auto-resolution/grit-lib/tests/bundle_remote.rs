//! Bundle remotes: fetch and clone compatibility with system `git`.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::progress::{ProgressPhase, ProgressSink, ProgressUnit};
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::{NoProgress, Progress};
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::remote::{Remote, RemoteUrl};
use grit_lib::repo::{InitOptions, Repository};
use std::collections::HashMap;

fn git_in(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_configure_user(dir: &Path) {
    assert!(git_in(dir, &["config", "user.email", "test@example.com"]));
    assert!(git_in(dir, &["config", "user.name", "Test"]));
}

fn init_with_commits(dir: &Path, messages: &[&str]) {
    assert!(git_in(dir, &["init", "-q", "-b", "main", "."]));
    git_configure_user(dir);
    for msg in messages {
        std::fs::write(dir.join("file.txt"), format!("{msg}\n")).unwrap();
        assert!(git_in(dir, &["add", "file.txt"]));
        assert!(git_in(dir, &["commit", "-qm", msg]));
    }
}

#[test]
fn fetch_from_git_bundle_matches_git_fetch() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_with_commits(upstream.path(), &["one", "two"]);
    let bundle_path = upstream.path().join("test.bundle");
    assert!(git_in(
        upstream.path(),
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/main",
        ],
    ));

    let dest = tempfile::tempdir().expect("dest");
    Repository::init(
        dest.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    git_configure_user(dest.path());
    assert!(git_in(
        dest.path(),
        &["commit", "--allow-empty", "-qm", "seed"],
    ));

    let remote = Remote::from_url(bundle_path.to_str().unwrap()).expect("bundle url");
    assert!(matches!(remote.fetch_url(), RemoteUrl::Bundle(_)));

    let repo = Repository::open(
        &dest.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dest.path()),
    )
    .unwrap();
    let mut progress = NoProgress;
    let outcome = remote
        .fetch(
            &repo,
            FetchOptions {
                refspecs: vec!["+refs/heads/main:refs/remotes/origin/main".to_owned()],
                tags: TagMode::None,
                ..Default::default()
            },
            &mut progress,
            None,
        )
        .expect("fetch bundle");

    assert!(git_in(
        dest.path(),
        &[
            "fetch",
            bundle_path.to_str().unwrap(),
            "refs/heads/main:refs/remotes/git/main",
        ],
    ));

    let grit_tip = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/heads/main")
        .and_then(|u| u.new_oid)
        .expect("grit fetched main");
    let git_tip = grit_lib::refs::resolve_ref(&dest.path().join(".git"), "refs/remotes/git/main")
        .expect("git tracking ref");
    assert_eq!(grit_tip, git_tip);
}

#[test]
fn literal_bundle_fetch_writes_fetch_head() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_with_commits(upstream.path(), &["bundle-tip"]);
    let bundle_path = upstream.path().join("inc.bundle");
    assert!(git_in(
        upstream.path(),
        &["bundle", "create", bundle_path.to_str().unwrap(), "HEAD",],
    ));

    let dest = tempfile::tempdir().expect("dest");
    Repository::init(
        dest.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    git_configure_user(dest.path());
    assert!(git_in(
        dest.path(),
        &["commit", "--allow-empty", "-qm", "seed"],
    ));

    let remote = Remote::from_url(bundle_path.to_str().unwrap()).expect("bundle url");
    let repo = Repository::open(
        &dest.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dest.path()),
    )
    .unwrap();
    remote
        .fetch(
            &repo,
            FetchOptions {
                tags: TagMode::None,
                ..Default::default()
            },
            &mut NoProgress,
            None,
        )
        .expect("anonymous bundle fetch");

    let grit_fetch_head = std::fs::read_to_string(dest.path().join(".git/FETCH_HEAD"))
        .expect("grit FETCH_HEAD must exist");
    let bundle_main =
        grit_lib::refs::resolve_ref(upstream.path().join(".git").as_path(), "refs/heads/main")
            .expect("upstream main");
    let grit_oid = grit_fetch_head.split('\t').next().expect("oid").trim();
    assert_eq!(grit_oid, bundle_main.to_hex());

    let git_only = tempfile::tempdir().expect("git only");
    Repository::init(
        git_only.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    git_configure_user(git_only.path());
    assert!(git_in(
        git_only.path(),
        &["commit", "--allow-empty", "-qm", "seed"],
    ));
    assert!(git_in(
        git_only.path(),
        &["fetch", bundle_path.to_str().unwrap()],
    ));
    let git_fetch_head =
        std::fs::read_to_string(git_only.path().join(".git/FETCH_HEAD")).expect("git FETCH_HEAD");
    let git_oid = git_fetch_head.split('\t').next().expect("git oid").trim();
    assert_eq!(grit_oid, git_oid);
}

#[test]
fn clone_from_bundle_matches_git_default_branch_when_head_not_first() {
    let upstream = tempfile::tempdir().expect("upstream");
    assert!(git_in(upstream.path(), &["init", "-q", "-b", "main", "."]));
    git_configure_user(upstream.path());
    std::fs::write(upstream.path().join("file.txt"), "base\n").unwrap();
    assert!(git_in(upstream.path(), &["add", "file.txt"]));
    assert!(git_in(upstream.path(), &["commit", "-qm", "base"]));
    assert!(git_in(upstream.path(), &["branch", "feature"]));
    assert!(git_in(upstream.path(), &["checkout", "feature"]));
    std::fs::write(upstream.path().join("file.txt"), "feature\n").unwrap();
    assert!(git_in(upstream.path(), &["add", "file.txt"]));
    assert!(git_in(upstream.path(), &["commit", "-qm", "feature-only"]));
    assert!(git_in(upstream.path(), &["checkout", "main"]));

    let bundle_path = upstream.path().join("multi.bundle");
    assert!(git_in(
        upstream.path(),
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/feature",
            "refs/heads/main",
            "HEAD",
        ],
    ));

    let git_dest = tempfile::tempdir().expect("git dest");
    assert!(git_in(
        git_dest.path(),
        &["clone", "-q", bundle_path.to_str().unwrap(), "repo"],
    ));
    let git_repo = git_dest.path().join("repo");
    let git_branch = Command::new("git")
        .current_dir(&git_repo)
        .args(["branch", "--show-current"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git branch");
    let git_branch = String::from_utf8(git_branch.stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    let git_head = Command::new("git")
        .current_dir(&git_repo)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-parse");
    let git_head = String::from_utf8(git_head.stdout)
        .expect("utf8")
        .trim()
        .to_owned();

    let grit_dest = tempfile::tempdir().expect("grit dest");
    let opts = CloneOptions {
        url: bundle_path.to_str().unwrap().to_owned(),
        dest: grit_dest.path().to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(
            bundle_path.to_str().unwrap(),
            grit_dest.path().to_path_buf(),
        )
    };
    let outcome = clone_repo(opts, &mut NoProgress, None).expect("grit clone from bundle");

    assert_eq!(outcome.branch, git_branch);
    assert_eq!(outcome.checkout_oid.to_hex(), git_head);
}

#[test]
fn clone_from_bundle_reports_transfer_stats() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_with_commits(upstream.path(), &["one"]);
    let bundle_path = upstream.path().join("stats.bundle");
    assert!(git_in(
        upstream.path(),
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/main",
        ],
    ));

    let dest = tempfile::tempdir().expect("dest");
    let opts = CloneOptions {
        url: bundle_path.to_str().unwrap().to_owned(),
        dest: dest.path().to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(bundle_path.to_str().unwrap(), dest.path().to_path_buf())
    };
    let outcome = clone_repo(opts, &mut NoProgress, None).expect("clone from bundle");

    let count_out = Command::new("git")
        .args(["count-objects", "-v"])
        .current_dir(dest.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("count-objects");
    assert!(count_out.status.success());
    let count_text = String::from_utf8_lossy(&count_out.stdout);
    let mut in_pack = None::<u64>;
    for line in count_text.lines() {
        if let Some(v) = line.strip_prefix("in-pack: ") {
            in_pack = Some(v.trim().parse().expect("in-pack"));
        }
    }
    let git_objects = in_pack.expect("in-pack");
    let git_dir = dest.path().join(".git");
    let pack_bytes = std::fs::read_dir(git_dir.join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .map(|p| std::fs::metadata(&p).expect("stat").len())
        .sum::<u64>();

    assert_eq!(outcome.transfer.objects, git_objects);
    assert_eq!(outcome.transfer.bytes, pack_bytes);
    assert!(outcome.transfer.objects > 0);
    assert!(outcome.transfer.bytes > 0);
}

#[test]
fn clone_from_bundle_passes_git_fsck() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_with_commits(upstream.path(), &["base"]);
    let bundle_path = upstream.path().join("clone.bundle");
    assert!(git_in(
        upstream.path(),
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/main",
        ],
    ));

    let dest = tempfile::tempdir().expect("dest");
    let opts = CloneOptions {
        url: bundle_path.to_str().unwrap().to_owned(),
        dest: dest.path().to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(bundle_path.to_str().unwrap(), dest.path().to_path_buf())
    };
    clone_repo(opts, &mut NoProgress, None).expect("clone from bundle");

    assert!(git_in(dest.path(), &["fsck"]));
}

#[derive(Debug, Default)]
struct PhaseRecorder {
    started: Vec<(ProgressPhase, ProgressUnit, Option<u64>)>,
    finished: Vec<ProgressPhase>,
    counts_by_phase: HashMap<ProgressPhase, Vec<u64>>,
    active: Option<ProgressPhase>,
    current: u64,
}

impl ProgressSink for PhaseRecorder {
    fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
        self.start_with_unit(phase, phase.default_unit(), total);
    }
    fn start_with_unit(&mut self, phase: ProgressPhase, unit: ProgressUnit, total: Option<u64>) {
        self.started.push((phase, unit, total));
        self.active = Some(phase);
        self.current = 0;
    }
    fn inc(&mut self, units: u64) {
        let Some(phase) = self.active else {
            return;
        };
        self.current += units;
        self.counts_by_phase
            .entry(phase)
            .or_default()
            .push(self.current);
    }
    fn set(&mut self, current: u64) {
        let Some(phase) = self.active else {
            return;
        };
        self.current = current;
        self.counts_by_phase
            .entry(phase)
            .or_default()
            .push(self.current);
    }
    fn finish(&mut self) {
        if let Some(phase) = self.active.take() {
            self.finished.push(phase);
        }
    }
}

impl Progress for PhaseRecorder {}

fn assert_monotonic(counts: &[u64]) {
    for pair in counts.windows(2) {
        assert!(pair[1] >= pair[0], "progress counts must be monotonic");
    }
}

fn phase_started_and_finished(rec: &PhaseRecorder, phase: ProgressPhase) -> bool {
    rec.started.iter().any(|(p, _, _)| *p == phase) && rec.finished.contains(&phase)
}

#[test]
fn bundle_clone_reports_progress() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_with_commits(upstream.path(), &["one", "two"]);
    let bundle_path = upstream.path().join("progress.bundle");
    assert!(git_in(
        upstream.path(),
        &[
            "bundle",
            "create",
            bundle_path.to_str().unwrap(),
            "refs/heads/main",
        ],
    ));

    let dest = tempfile::tempdir().expect("dest");
    let opts = CloneOptions {
        url: bundle_path.to_str().unwrap().to_owned(),
        dest: dest.path().to_path_buf(),
        environment: Environment::empty(),
        ..CloneOptions::new(bundle_path.to_str().unwrap(), dest.path().to_path_buf())
    };
    let mut rec = PhaseRecorder::default();
    clone_repo(opts, &mut rec, None).expect("clone from bundle");

    assert!(
        phase_started_and_finished(&rec, ProgressPhase::ReceivingObjects),
        "expected ReceivingObjects start/finish"
    );
    assert!(
        phase_started_and_finished(&rec, ProgressPhase::IndexingObjects),
        "expected IndexingObjects start/finish"
    );
    let receive_unit = rec
        .started
        .iter()
        .find(|(p, _, _)| *p == ProgressPhase::ReceivingObjects)
        .map(|(_, u, _)| *u)
        .expect("receive phase");
    assert_eq!(receive_unit, ProgressUnit::Bytes);
    let receive_counts = rec
        .counts_by_phase
        .get(&ProgressPhase::ReceivingObjects)
        .expect("receive byte counts");
    assert!(!receive_counts.is_empty());
    assert_monotonic(receive_counts);
    let index_counts = rec
        .counts_by_phase
        .get(&ProgressPhase::IndexingObjects)
        .expect("indexing object counts");
    assert!(!index_counts.is_empty());
    assert_monotonic(index_counts);
    assert!(
        index_counts.last().copied().unwrap_or(0) > 0,
        "indexing should report indexed objects"
    );
}

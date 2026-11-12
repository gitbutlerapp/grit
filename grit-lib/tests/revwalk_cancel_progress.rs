//! Cancel and progress reporting for long-running revwalk reads.

use grit_lib::blame::BlameOptions;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectKind;
use grit_lib::progress::{CancelToken, NullProgress, ProgressPhase, ProgressSink};
use grit_lib::refs::store::RefName;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::RevListOptions;
use grit_lib::revwalk::DescribeOptions;
use std::path::Path;
use tempfile::tempdir;

fn empty_tree(repo: &Repository) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    repo.odb().write_loose_materialize(ObjectKind::Tree, b"")
}

fn write_commit(
    repo: &Repository,
    parent: Option<grit_lib::objects::ObjectId>,
    n: usize,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let tree = empty_tree(repo)?;
    let mut body = format!("tree {tree}\n");
    if let Some(p) = parent {
        body.push_str(&format!("parent {p}\n"));
    }
    let t = 1_700_000_000 + n as i64;
    body.push_str(&format!(
        "author T <t@example.com> {t} +0000\ncommitter T <t@example.com> {t} +0000\n\n{n}\n"
    ));
    repo.odb()
        .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn init_repo(root: &Path, commits: usize) -> (Repository, grit_lib::objects::ObjectId) {
    Repository::init(
        root,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let repo = Repository::open(root, &RepositoryOptions::empty()).expect("open");
    let mut tip = write_commit(&repo, None, 0).expect("root");
    for i in 1..commits {
        tip = write_commit(&repo, Some(tip), i).expect("commit");
    }
    repo.references()
        .update(&RefName::try_from("refs/heads/main").unwrap(), &tip)
        .expect("update-ref");
    (repo, tip)
}

#[derive(Default)]
struct PhaseRecorder {
    started: Vec<ProgressPhase>,
    object_progress: u64,
}

impl ProgressSink for PhaseRecorder {
    fn start(&mut self, phase: ProgressPhase, _total: Option<u64>) {
        self.started.push(phase);
    }
    fn set(&mut self, current: u64) {
        self.object_progress = current;
    }
    fn finish(&mut self) {}
}

#[test]
fn rev_list_objects_cancelled_mid_walk() {
    let dir = tempdir().expect("tempdir");
    let (repo, _) = init_repo(dir.path(), 300);
    let cancel = CancelToken::after_polls(1);
    let opts = RevListOptions::default().with_objects(true);
    let err = repo
        .revwalk()
        .rev_list(
            &[String::from("main")],
            &[],
            &opts,
            &mut NullProgress,
            &cancel,
        )
        .expect_err("cancelled");
    assert!(matches!(err, Error::Cancelled));
}

#[test]
fn rev_list_reports_walking_history_phase() {
    let dir = tempdir().expect("tempdir");
    let (repo, _) = init_repo(dir.path(), 300);
    let mut recorder = PhaseRecorder::default();
    let opts = RevListOptions::default();
    repo.revwalk()
        .rev_list(
            &[String::from("main")],
            &[],
            &opts,
            &mut recorder,
            &CancelToken::never(),
        )
        .expect("rev_list");
    assert!(
        recorder.started.contains(&ProgressPhase::WalkingHistory),
        "started phases: {:?}",
        recorder.started
    );
    assert!(
        recorder.object_progress > 0,
        "expected strided object progress updates"
    );
}

#[test]
fn uncancelled_rev_list_matches_commit_chain() {
    let dir = tempdir().expect("tempdir");
    let (repo, tip) = init_repo(dir.path(), 10);
    let listed = repo
        .revwalk()
        .rev_list(
            &[String::from("main")],
            &[],
            &RevListOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("rev_list");
    assert_eq!(listed.commits.len(), 10);
    assert_eq!(listed.commits[0], tip);
}

fn tree_one_file(
    repo: &Repository,
    path: &str,
    blob: grit_lib::objects::ObjectId,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let mut body = format!("100644 {path}\0").into_bytes();
    body.extend_from_slice(blob.as_bytes());
    repo.odb().write_loose_materialize(ObjectKind::Tree, &body)
}

fn write_file_commit(
    repo: &Repository,
    parent: Option<grit_lib::objects::ObjectId>,
    path: &str,
    content: &[u8],
    n: usize,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let blob = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, content)?;
    let tree = tree_one_file(repo, path, blob)?;
    let mut body = format!("tree {tree}\n");
    if let Some(p) = parent {
        body.push_str(&format!("parent {p}\n"));
    }
    let t = 1_700_000_000 + n as i64;
    body.push_str(&format!(
        "author T <t@example.com> {t} +0000\ncommitter T <t@example.com> {t} +0000\n\n{n}\n"
    ));
    repo.odb()
        .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

#[test]
fn describe_cancelled_mid_walk() {
    let dir = tempdir().expect("tempdir");
    let (repo, tip) = init_repo(dir.path(), 300);
    let mid = listed_nth(&repo, tip, 150);
    repo.references()
        .update(&RefName::try_from("refs/tags/mid").unwrap(), &mid)
        .expect("tag");
    let cancel = CancelToken::after_polls(1);
    let opts = DescribeOptions::default().with_tags(true);
    let err = repo
        .revwalk()
        .describe("HEAD", &opts, &mut NullProgress, &cancel)
        .expect_err("cancelled");
    assert!(matches!(err, Error::Cancelled));
}

fn listed_nth(
    repo: &Repository,
    _tip: grit_lib::objects::ObjectId,
    n: usize,
) -> grit_lib::objects::ObjectId {
    repo.revwalk()
        .rev_list(
            &[String::from("HEAD")],
            &[],
            &RevListOptions::default().with_max_count(Some(n + 1)),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("rev_list")
        .commits
        .last()
        .copied()
        .expect("nth commit")
}

#[test]
fn blame_reports_blaming_phase() {
    let dir = tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        dir.path(),
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    std::fs::write(dir.path().join("f.txt"), b"line\n").expect("write");
    let mut parent = None;
    for i in 0..3 {
        let tip = write_file_commit(&repo, parent, "f.txt", b"line\n", i).expect("commit");
        parent = Some(tip);
    }
    let tip = parent.unwrap();
    repo.references()
        .update(&RefName::try_from("refs/heads/main").unwrap(), &tip)
        .expect("ref");
    let mut recorder = PhaseRecorder::default();
    repo.diff()
        .blame(
            "f.txt",
            &BlameOptions::default(),
            &mut recorder,
            &CancelToken::never(),
        )
        .expect("blame");
    assert!(recorder.started.contains(&ProgressPhase::Blaming));
}

#[test]
fn describe_reports_describing_phase() {
    let dir = tempdir().expect("tempdir");
    let (repo, tip) = init_repo(dir.path(), 300);
    let mid = listed_nth(&repo, tip, 100);
    repo.references()
        .update(&RefName::try_from("refs/tags/mid").unwrap(), &mid)
        .expect("tag");
    let mut recorder = PhaseRecorder::default();
    let opts = DescribeOptions::default().with_tags(true);
    repo.revwalk()
        .describe("HEAD", &opts, &mut recorder, &CancelToken::never())
        .expect("describe");
    assert!(recorder.started.contains(&ProgressPhase::Describing));
}

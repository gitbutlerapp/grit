//! Repository-level conformance for object-store backends.
//!
//! Wire a [`BackendFixture`] into [`run_repository_suite`] to exercise init, porcelain,
//! merge/replay, notes, stash, fetch, and fsck through the public `grit-lib` API. Use
//! [`run_parity`] to assert two backends produce identical typed [`RepositorySnapshot`]
//! outcomes for the same scripted history.
//!
//! ```no_run
//! use grit_lib::repo::InitOptions;
//! use grit_test_support::repository_conformance::{run_repository_suite, BackendFixture};
//!
//! struct Files;
//! impl BackendFixture for Files { /* … */ }
//! run_repository_suite(&Files);
//! ```

#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::change_id::{ChangeId, ChangeIdError, ChangeIdGenerator};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::{ConfigEditor, ConfigScope, ConfigWriteOptions};
use grit_lib::diff::{hunk_ids_with_duplicate_peers, DiffSide};
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::notes::NotesRef;
use grit_lib::objects::HashAlgo;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::porcelain::hunk_select::unstaged_file_diffs;
use grit_lib::porcelain::log::DecorationOptions;
use grit_lib::porcelain::merge::{abort_branch_merge, integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    start_replay_sequence, ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::store::RefName;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec};
use grit_lib::remote::Remote;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::NotesWriteOptions;
use grit_lib::revwalk::rev_list::RevListOptions;
use grit_lib::stash::{StashApplyOptions, StashPushOptions};

use crate::system_git_version;

/// Deterministic author/committer for conformance scenarios.
pub const CONFORMANCE_IDENT: &str = "A U Thor <author@example.com> 1700000000 +0000";

struct CountingChangeIdGenerator {
    next: std::sync::atomic::AtomicU64,
    salt: [u8; 8],
}

impl CountingChangeIdGenerator {
    fn new(salt: u8) -> Self {
        Self {
            next: std::sync::atomic::AtomicU64::new(0),
            salt: [salt; 8],
        }
    }
}

impl ChangeIdGenerator for CountingChangeIdGenerator {
    fn random_bytes(&self) -> std::result::Result<[u8; 16], ChangeIdError> {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&n.to_le_bytes());
        out[8..].copy_from_slice(&self.salt);
        Ok(out)
    }
}

/// Options every conformance scenario shares so object ids match across backends.
#[must_use]
pub fn suite_init_options() -> InitOptions {
    let generator: Arc<dyn ChangeIdGenerator> = Arc::new(CountingChangeIdGenerator::new(0xA5));
    let mut repository = RepositoryOptions::empty().with_change_id_generator(generator);
    repository.reference_unix_time = Some(1_700_000_000);
    InitOptions::new()
        .with_initial_branch("main")
        .with_repository_options(repository)
}

fn signature_at(epoch: i64) -> String {
    format!("A U Thor <author@example.com> {epoch} +0000")
}

fn commit_request(message: &str, epoch: i64) -> CommitRequest {
    let ident: grit_lib::ident::Signature = signature_at(epoch).parse().expect("signature");
    let mut req = CommitRequest::new(message.to_owned());
    req.author = Some(ident.clone());
    req.committer = Some(ident);
    req
}

fn configure_local_identity(repo: &Repository) {
    let editor = ConfigEditor::open(
        &grit_lib::config::environment::Environment::empty(),
        repo.git_dir().join("config"),
        ConfigScope::Local,
    );
    let opts = ConfigWriteOptions::new();
    editor
        .set("user.name", "A U Thor", &opts)
        .expect("user.name");
    editor
        .set("user.email", "author@example.com", &opts)
        .expect("user.email");
    editor
        .set("grit.testing.changeId", "content-hash", &opts)
        .expect("testing.changeId");
}

/// Pluggable repository backend under test.
pub trait BackendFixture: Send + Sync {
    /// Short name (`files`, `memory`, …).
    fn name(&self) -> &str;

    /// Create and open a non-bare repository at `dir` using shared [`suite_init_options`].
    fn init(&self, dir: &Path, opts: &InitOptions) -> Repository;

    /// Materialize `repo` into a normal on-disk repository at `dest` (work tree parent).
    fn export_to_files(&self, repo: &Repository, dest: &Path);
}

fn work_tree(repo: &Repository) -> &Path {
    repo.work_tree().expect("non-bare conformance repo")
}

fn head_oid(repo: &Repository, branch: &str) -> ObjectId {
    let name = format!("refs/heads/{branch}");
    repo.references()
        .resolve(RefName::try_from(name.as_str()).expect("ref"))
        .expect("resolve head")
}

fn switch(repo: &Repository, mode: SwitchMode) {
    switch_repository(
        repo,
        &SwitchOptions {
            mode,
            dry_run: false,
            hooks: Default::default(),
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("switch");
}

fn commit_path(repo: &Repository, path: &str, contents: &str, message: &str, epoch: i64) {
    fs::write(work_tree(repo).join(path), contents).expect("write");
    stage(
        repo,
        &StageOptions {
            pathspecs: vec![path.into()],
            ..Default::default()
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage");
    commit_worktree(
        repo,
        &commit_request(message, epoch),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit");
}

fn log_main(repo: &Repository) -> Vec<(ObjectId, String, ChangeId)> {
    let revwalk = repo.revwalk();
    let walked = revwalk
        .rev_list(&["HEAD".to_owned()], &[], &RevListOptions::default())
        .expect("rev-list");
    let decorations = revwalk
        .decorations(&DecorationOptions::default())
        .expect("decorations");
    revwalk
        .load_commits(&walked.commits, &decorations, &mut ())
        .expect("load")
        .into_iter()
        .map(|c| {
            let subject = c
                .message
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_owned();
            (c.oid, subject, c.change_id)
        })
        .collect()
}

fn unstaged_hunk_hex_ids(repo: &Repository, path: &str) -> Vec<String> {
    let files = unstaged_file_diffs(repo, &[path.to_owned()]).expect("unstaged diff");
    if files.is_empty() {
        return Vec::new();
    }
    let (file, peer) = &files[0];
    hunk_ids_with_duplicate_peers(DiffSide::Unstaged, &file.path, &file.hunks, peer)
        .into_iter()
        .map(|id| id.as_str())
        .collect()
}

fn object_hex_set(repo: &Repository) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    repo.odb()
        .for_each_object(&mut |oid| {
            set.insert(oid.to_hex());
            ControlFlow::Continue(())
        })
        .expect("for_each_object");
    set
}

fn ref_map(repo: &Repository) -> BTreeMap<String, String> {
    repo.references()
        .list("refs/")
        .expect("list refs")
        .into_iter()
        .map(|(name, oid)| (name.as_str().to_owned(), oid.to_hex()))
        .collect()
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir dest");
    for entry in fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// Export every object of `repo` into `dest/.git/objects` (work tree is copied when present).
pub fn export_objects_to_files_repo(repo: &Repository, dest: &Path) {
    if dest.exists() {
        fs::remove_dir_all(dest).expect("clear export dest");
    }
    if let Some(wt) = repo.work_tree() {
        copy_tree(wt, dest);
    } else {
        fs::create_dir_all(dest).expect("mkdir");
    }
    let git_dir = dest.join(".git");
    fs::create_dir_all(git_dir.join("objects")).expect("objects");
    let disk_repo = Repository::open(&git_dir, &RepositoryOptions::empty().with_work_tree(dest))
        .expect("open export");
    repo.odb()
        .for_each_object(&mut |oid| {
            let obj = repo.odb().read(oid).expect("read");
            let written = disk_repo
                .odb()
                .write(obj.kind, &obj.data)
                .expect("write export");
            assert_eq!(written, *oid);
            ControlFlow::Continue(())
        })
        .expect("export");
}

/// Typed outcomes of the shared conformance scenario (for parity diffs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub main_history: Vec<(ObjectId, String, ChangeId)>,
    pub feature_tip: ObjectId,
    pub unstaged_hunk_ids: Vec<String>,
    pub staged_paths: Vec<String>,
    pub unstaged_paths: Vec<String>,
    pub untracked_paths: Vec<String>,
    pub clean_merge_tip: ObjectId,
    pub conflict_paths: Vec<String>,
    pub pick_tip: ObjectId,
    pub revert_tip: ObjectId,
    pub blame_commits: Vec<ObjectId>,
    pub rev_parse_short: String,
    pub change_id_prefix: String,
    pub note_body: Vec<u8>,
    pub stash_tip: ObjectId,
    pub pushed_main_tip: ObjectId,
    pub fetched_tip: ObjectId,
    pub object_set: BTreeSet<String>,
    pub refs_after_fetch: BTreeMap<String, String>,
    pub fsck_clean: bool,
}

/// Build the full scenario on a fresh repository from `fixture`.
pub fn build_snapshot(fixture: &impl BackendFixture) -> RepositorySnapshot {
    let dir = tempfile::tempdir().expect("tempdir");
    let opts = suite_init_options();
    let repo = fixture.init(dir.path(), &opts);
    configure_local_identity(&repo);
    run_snapshot_on_repo(&repo, fixture, dir.path())
}

fn run_snapshot_on_repo(
    repo: &Repository,
    fixture: &impl BackendFixture,
    root: &Path,
) -> RepositorySnapshot {
    commit_path(repo, "story.txt", "line1\n", "c1", 1_700_000_000);
    commit_path(repo, "story.txt", "line1\nline2\n", "c2", 1_700_000_001);
    commit_path(
        repo,
        "story.txt",
        "line1\nline2\nline3\n",
        "c3",
        1_700_000_002,
    );
    let c3 = head_oid(repo, "main");

    switch(
        repo,
        SwitchMode::Create {
            branch: "feature".into(),
            start_oid: c3,
            create_source_label: "main".into(),
            track_upstream: None,
        },
    );
    commit_path(repo, "feature.txt", "feature\n", "feature", 1_700_000_003);
    let feature_tip = head_oid(repo, "feature");

    switch(
        repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: c3,
        },
    );
    let main_history = log_main(repo);

    commit_path(repo, "main-only.txt", "main\n", "main line", 1_700_000_010);
    let main_before_merge = head_oid(repo, "main");

    let merge_out = integrate_branch(
        repo,
        "refs/heads/main",
        main_before_merge,
        feature_tip,
        "feature",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("merge");
    let clean_merge_tip = match merge_out {
        IntegrateBranchOutcome::Merged { oid, .. } => oid,
        other => panic!("expected clean merge, got {other:?}"),
    };

    switch(
        repo,
        SwitchMode::Create {
            branch: "pick-base".into(),
            start_oid: clean_merge_tip,
            create_source_label: "main".into(),
            track_upstream: None,
        },
    );
    commit_path(repo, "pick.txt", "picked\n", "pick me", 1_700_000_030);
    let pick_src = head_oid(repo, "pick-base");
    switch(
        repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: clean_merge_tip,
        },
    );
    let pick_out = start_replay_sequence(
        repo,
        &ReplaySequenceRequest {
            commits: vec![pick_src],
            direction: ReplayDirection::Pick,
            committer: signature_at(1_700_000_031).parse().expect("sig"),
            options: ReplayOptions::default(),
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("pick");
    let pick_tip = match pick_out {
        SequenceOutcome::Completed { created, .. } => {
            assert_eq!(created.len(), 1);
            created[0].new_oid
        }
        other => panic!("pick: {other:?}"),
    };

    commit_path(
        repo,
        "revert.txt",
        "to revert\n",
        "revert me",
        1_700_000_040,
    );
    let revert_src = head_oid(repo, "main");
    let revert_out = start_replay_sequence(
        repo,
        &ReplaySequenceRequest {
            commits: vec![revert_src],
            direction: ReplayDirection::Revert,
            committer: signature_at(1_700_000_041).parse().expect("sig"),
            options: ReplayOptions::default(),
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("revert");
    let revert_tip = match revert_out {
        SequenceOutcome::Completed { created, .. } => {
            assert_eq!(created.len(), 1);
            created[0].new_oid
        }
        other => panic!("revert: {other:?}"),
    };

    switch(
        repo,
        SwitchMode::Create {
            branch: "conflict".into(),
            start_oid: c3,
            create_source_label: "main".into(),
            track_upstream: None,
        },
    );
    commit_path(
        repo,
        "shared.txt",
        "topic side\n",
        "topic edit",
        1_700_000_020,
    );
    let topic_conflict = head_oid(repo, "conflict");
    switch(
        repo,
        SwitchMode::Existing {
            branch: "main".into(),
            oid: head_oid(repo, "main"),
        },
    );
    commit_path(
        repo,
        "shared.txt",
        "main side\n",
        "main edit",
        1_700_000_021,
    );
    let main_conflict = head_oid(repo, "main");
    let conflict_out = integrate_branch(
        repo,
        "refs/heads/main",
        main_conflict,
        topic_conflict,
        "conflict",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("conflict merge");
    let conflict_paths = match conflict_out {
        IntegrateBranchOutcome::Conflicts { paths, .. } => paths,
        other => panic!("expected conflicts, got {other:?}"),
    };
    abort_branch_merge(repo).expect("abort");

    let blame =
        grit_lib::blame::blame_file(repo, "story.txt", &grit_lib::blame::BlameOptions::default())
            .expect("blame");
    let blame_commits: Vec<_> = blame.lines.iter().map(|l| l.commit).collect();

    let short = repo
        .revwalk()
        .resolve_commits(&["HEAD~1".to_owned()])
        .expect("rev-parse")[0]
        .to_hex();
    let rev_parse_short = short.chars().take(7).collect();
    let change_id_prefix = main_history
        .first()
        .map(|(_, _, id)| id.as_str().chars().take(8).collect())
        .unwrap_or_default();

    let note_target = head_oid(repo, "main");
    let notes = repo.notes();
    let write_opts = NotesWriteOptions {
        message: "add conformance note".into(),
        committer: signature_at(1_700_000_050).parse().expect("sig"),
        author: None,
    };
    notes
        .add(
            &NotesRef::commits(),
            &note_target,
            b"conformance note\n",
            &write_opts,
            false,
        )
        .expect("notes add");
    let note_body = notes
        .read(&NotesRef::commits(), &note_target)
        .expect("read note")
        .expect("note");

    fs::write(work_tree(repo).join("staged-only.txt"), "staged\n").expect("write");
    stage(
        repo,
        &StageOptions {
            pathspecs: vec!["staged-only.txt".into()],
            ..Default::default()
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage file");
    fs::write(work_tree(repo).join("untracked.txt"), "new\n").expect("untracked");
    fs::write(
        work_tree(repo).join("story.txt"),
        "line1\nline2\nline3\nwip\n",
    )
    .expect("wip");
    let unstaged_hunk_ids = unstaged_hunk_hex_ids(repo, "story.txt");
    let model = status(
        repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("status");
    let staged_paths: Vec<_> = model.staged.iter().map(|e| e.path().to_owned()).collect();
    let unstaged_paths: Vec<_> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();

    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage for stash");
    let stash_out = repo
        .stash()
        .push(
            &StashPushOptions::new(signature_at(1_700_000_060).parse().expect("sig")),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stash push")
        .expect("stash created");
    let stash_tip = stash_out.oid;

    repo.stash()
        .pop(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stash pop");

    let bare = root.join("remote.git");
    fs::create_dir_all(&bare).expect("bare");
    Repository::init(
        &bare,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main"),
    )
    .expect("init bare");
    let bare_repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open bare");
    let mut progress = NullProgress;
    repo.remotes()
        .add("origin", bare.to_str().expect("utf8"))
        .expect("add remote");
    repo.reload_config().expect("reload");
    let pushed_main_tip = head_oid(repo, "main");
    let origin = Remote::from_url(bare.to_str().expect("utf8")).expect("origin url");
    repo.remotes()
        .push(
            &origin,
            &[PushRefSpec {
                src: Some(pushed_main_tip),
                src_ref: None,
                dst: "refs/heads/main".into(),
                force: false,
                delete: false,
                expected_old: None,
                expect_absent: false,
                ..Default::default()
            }],
            PushOptions::default(),
            &mut progress,
            &CancelToken::never(),
            None,
        )
        .expect("push");

    let fetch_dir = root.join("fetch-dest");
    fs::create_dir_all(&fetch_dir).expect("mkdir");
    let fetch_repo = fixture.init(&fetch_dir, &suite_init_options());
    configure_local_identity(&fetch_repo);
    fetch_repo
        .remotes()
        .add("origin", bare.to_str().expect("utf8"))
        .expect("remote");
    fetch_repo.reload_config().expect("reload");
    let origin = Remote::from_url(bare.to_str().expect("utf8")).expect("origin url");
    let mut fetch_opts = FetchOptions::default();
    fetch_opts.remote_name = Some("origin".into());
    fetch_opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".into()];
    fetch_repo
        .remotes()
        .fetch(
            &origin,
            fetch_opts,
            &mut NullProgress,
            &CancelToken::never(),
            None,
        )
        .expect("fetch");
    let fetched_tip = fetch_repo
        .references()
        .resolve(RefName::try_from("refs/remotes/origin/main").expect("ref"))
        .expect("fetched");
    let refs_after_fetch = ref_map(&fetch_repo);

    let fsck = repo
        .fsck(
            &FsckOptions::full_repository(true),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("fsck");
    let fsck_clean = fsck.issues.is_empty();

    let _ = bare_repo;

    RepositorySnapshot {
        main_history,
        feature_tip,
        unstaged_hunk_ids,
        staged_paths,
        unstaged_paths,
        untracked_paths: model.untracked,
        clean_merge_tip,
        conflict_paths,
        pick_tip,
        revert_tip,
        blame_commits,
        rev_parse_short,
        change_id_prefix,
        note_body,
        stash_tip,
        pushed_main_tip,
        fetched_tip,
        object_set: object_hex_set(repo),
        refs_after_fetch,
        fsck_clean,
    }
}

/// Assert two backends yield the same [`RepositorySnapshot`].
pub fn run_parity(a: &impl BackendFixture, b: &impl BackendFixture) {
    let left = build_snapshot(a);
    let right = build_snapshot(b);
    assert_eq!(left, right, "parity: {} vs {}", a.name(), b.name());
}

/// Run every conformance case on `fixture`.
pub fn run_repository_suite(fixture: &impl BackendFixture) {
    macro_rules! run {
        ($name:expr, $case:expr) => {
            $case(fixture);
            eprintln!("repository_conformance::{} ({})", $name, fixture.name());
        };
    }
    run!("init_non_bare", case_init_non_bare);
    run!("history_and_log", case_history_and_log);
    run!("diff_hunks_and_status", case_diff_and_status);
    run!("merge_clean", case_merge_clean);
    run!("merge_conflict_abort", case_merge_conflict_abort);
    run!("pick_and_revert", case_pick_and_revert);
    run!("blame", case_blame);
    run!("rev_parse_and_change_id", case_rev_parse);
    run!("notes", case_notes);
    run!("stash", case_stash);
    run!("fetch_roundtrip", case_fetch);
    run!("fsck_clean", case_fsck);
    run!("git_export_roundtrip", case_git_roundtrip);
}

fn case_init_non_bare(fixture: &impl BackendFixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = fixture.init(dir.path(), &suite_init_options());
    assert!(repo.work_tree().is_some());
    assert!(dir.path().join(".git").is_dir());
}

fn case_history_and_log(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_eq!(snap.main_history.len(), 3);
    assert_eq!(snap.main_history[0].1, "c3");
    assert_ne!(snap.feature_tip, snap.main_history[0].0);
}

fn case_diff_and_status(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert!(!snap.unstaged_hunk_ids.is_empty());
    assert!(snap.staged_paths.iter().any(|p| p == "staged-only.txt"));
    assert!(snap.untracked_paths.iter().any(|p| p == "untracked.txt"));
}

fn case_merge_clean(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_ne!(snap.clean_merge_tip, ObjectId::null(HashAlgo::Sha1));
}

fn case_merge_conflict_abort(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert!(snap.conflict_paths.iter().any(|p| p == "shared.txt"));
}

fn case_pick_and_revert(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_ne!(snap.pick_tip, snap.revert_tip);
}

fn case_blame(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_eq!(snap.blame_commits.len(), 3);
}

fn case_rev_parse(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_eq!(snap.rev_parse_short.len(), 7);
    assert!(!snap.change_id_prefix.is_empty());
}

fn case_notes(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_eq!(snap.note_body, b"conformance note\n");
}

fn case_stash(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_ne!(snap.stash_tip, ObjectId::null(HashAlgo::Sha1));
}

fn case_fetch(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_eq!(
        snap.fetched_tip, snap.pushed_main_tip,
        "fetch should match push"
    );
    assert!(snap
        .refs_after_fetch
        .contains_key("refs/remotes/origin/main"));
}

fn case_fsck(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert!(snap.fsck_clean);
}

fn case_git_roundtrip(fixture: &impl BackendFixture) {
    if system_git_version().is_none() {
        eprintln!("skip git_export_roundtrip: system git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let opts = suite_init_options();
    let repo = fixture.init(dir.path(), &opts);
    configure_local_identity(&repo);
    commit_path(&repo, "story.txt", "line1\n", "c1", 1_700_000_000);
    commit_path(&repo, "story.txt", "line1\nline2\n", "c2", 1_700_000_001);
    let export_root = tempfile::tempdir().expect("export");
    fixture.export_to_files(&repo, export_root.path());

    let fsck = Command::new("git")
        .current_dir(export_root.path())
        .args(["fsck", "--strict", "--full"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    let git_log = Command::new("git")
        .current_dir(export_root.path())
        .args(["log", "--format=%H", "--all"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git log");
    assert!(git_log.status.success());
    let git_hashes: BTreeSet<_> = String::from_utf8_lossy(&git_log.stdout)
        .lines()
        .map(str::to_owned)
        .collect();

    let revwalk = repo.revwalk();
    let walked = revwalk
        .rev_list(&["HEAD".to_owned()], &[], &RevListOptions::default())
        .expect("rev-list");
    let grit_hashes: BTreeSet<_> = walked.commits.iter().map(|o| o.to_hex()).collect();
    assert_eq!(git_hashes, grit_hashes);
}

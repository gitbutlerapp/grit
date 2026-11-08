//! Repository-level conformance for object-store backends.
//!
//! Wire a [`BackendFixture`] into [`run_repository_suite`] to exercise init, porcelain,
//! merge/replay, notes, stash, fetch, and fsck through the public `grit-lib` API. Use
//! [`run_parity`] to assert two backends produce identical typed [`RepositorySnapshot`]
//! outcomes for the same scripted history.
//!
//! ```no_run
//! use std::path::Path;
//! use grit_lib::repo::{InitOptions, Repository};
//! use grit_test_support::repository_conformance::{
//!     export_objects_to_files_repo, run_repository_suite, BackendFixture,
//! };
//!
//! struct Files;
//! impl BackendFixture for Files {
//!     fn name(&self) -> &str {
//!         "files"
//!     }
//!     fn init(&self, dir: &Path, opts: &InitOptions) -> Repository {
//!         Repository::init(dir, opts).expect("init").0
//!     }
//!     fn export_to_files(&self, repo: &Repository, dest: &Path) {
//!         export_objects_to_files_repo(repo, dest);
//!     }
//! }
//! run_repository_suite(&Files);
//! ```

#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::change_id::{
    resolve_commit_by_change_id_prefix, ChangeId, ChangeIdError, ChangeIdGenerator,
};
use grit_lib::commits::{CommitOptions, CommitOutcome, CommitRequest, CommitScope};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::{ConfigEditor, ConfigScope, ConfigWriteOptions};
use grit_lib::diff::structured::{StructuredHunk, StructuredLineKind};
use grit_lib::diff::{hunk_ids, DiffOptions, DiffSide, DiffTarget};
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::merge::{
    BranchIntegrateRequest, IntegrateBranchOutcome, MergeOptions, MergeSource, MergeSourceKind,
    ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
};
use grit_lib::notes::NotesRef;
use grit_lib::objects::HashAlgo;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{
    conflict_kind_from_stagemask, conflicts_from_index, Conflict, ConflictKind,
};
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::log::DecorationOptions;
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::switch::{SwitchMode, SwitchOptions};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::store::RefName;
use grit_lib::remote::transfer::{FetchOptions, PushOptions, PushRefSpec};
use grit_lib::remote::Remote;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::NotesWriteOptions;
use grit_lib::revwalk::rev_list::RevListOptions;
use grit_lib::stash::{StashApplyOptions, StashPushOptions};
use grit_lib::status::StatusOptions;
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};

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
    repo.checkout()
        .switch(
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

fn commit_path(
    repo: &Repository,
    path: &str,
    contents: &str,
    message: &str,
    epoch: i64,
) -> CommitOutcome {
    fs::write(work_tree(repo).join(path), contents).expect("write");
    repo.index()
        .stage(
            &StageOptions {
                pathspecs: vec![path.into()],
                ..Default::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stage");
    repo.commits()
        .create(
            &CommitOptions::new(commit_request(message, epoch), CommitScope::AutoStage),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("commit")
}

fn commit_history_row(out: &CommitOutcome) -> (ObjectId, String, ChangeId) {
    let subject = out
        .message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_owned();
    (
        out.oid,
        subject,
        out.change_id
            .clone()
            .expect("conformance commit records change-id"),
    )
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

fn pathspec_one(path: &str) -> Pathspec {
    Pathspec::scoped([path], PathspecGlobalFlags::default())
}

fn unstaged_hunk_hex_ids(repo: &Repository, path: &str) -> Vec<String> {
    let files = repo
        .index()
        .unstaged_file_diffs(&[path.to_owned()])
        .expect("unstaged diff");
    if files.is_empty() {
        return Vec::new();
    }
    let (file, peer) = &files[0];
    hunk_ids(DiffSide::Unstaged, &file.path, &file.hunks, peer)
        .into_iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

fn commit_file_hunks(repo: &Repository, commit: ObjectId, path: &str) -> Vec<StructuredHunk> {
    let files = repo
        .diff()
        .files(
            &DiffTarget::Commit(commit),
            &pathspec_one(path),
            &DiffOptions::default(),
        )
        .expect("commit file diff");
    files
        .iter()
        .find(|file| file.path == path)
        .map(|file| file.hunks.clone())
        .unwrap_or_default()
}

fn commit_file_hunk_hex_ids(repo: &Repository, commit: ObjectId, path: &str) -> Vec<String> {
    let hunks = commit_file_hunks(repo, commit, path);
    hunk_ids(DiffSide::Other, path, &hunks, &[])
        .into_iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

/// Line-level summary retained for parity (not just stable hunk ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceHunkSummary {
    pub old_start: usize,
    pub new_start: usize,
    pub old_lines: usize,
    pub new_lines: usize,
    pub removed: Vec<String>,
    pub added: Vec<String>,
}

fn hunk_summaries(hunks: &[StructuredHunk]) -> Vec<ConformanceHunkSummary> {
    hunks
        .iter()
        .map(|hunk| {
            let mut removed = Vec::new();
            let mut added = Vec::new();
            for line in &hunk.lines {
                let text: String = line
                    .segments
                    .iter()
                    .map(|segment| segment.text.as_str())
                    .collect();
                match line.kind {
                    StructuredLineKind::Del => removed.push(text),
                    StructuredLineKind::Add => added.push(text),
                    StructuredLineKind::Context => {}
                }
            }
            ConformanceHunkSummary {
                old_start: hunk.old_start,
                new_start: hunk.new_start,
                old_lines: hunk.old_lines,
                new_lines: hunk.new_lines,
                removed,
                added,
            }
        })
        .collect()
}

fn index_head_hunk_hex_ids(repo: &Repository, path: &str) -> Vec<String> {
    let files = repo
        .diff()
        .files(
            &DiffTarget::IndexVsHead,
            &pathspec_one(path),
            &DiffOptions::default(),
        )
        .expect("staged diff");
    if files.is_empty() {
        return Vec::new();
    }
    hunk_ids(DiffSide::Staged, &files[0].path, &files[0].hunks, &[])
        .into_iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

fn init_files_primary_repo(dir: &Path) -> Repository {
    let (repo, _) = Repository::init(dir, &suite_init_options()).expect("init files");
    repo
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

fn remote_tracking_refs(map: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    map.iter()
        .filter(|(name, _)| name.starts_with("refs/remotes/origin/"))
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect()
}

fn path_tracked_in_index(repo: &Repository, path: &str) -> bool {
    let index = repo
        .index()
        .load(
            &grit_lib::index::IndexLoadOptions::default()
                .with_sparse(grit_lib::index::SparseIndexLoad::Preserve),
        )
        .expect("index");
    let path_bytes = path.as_bytes();
    index
        .entries()
        .iter()
        .any(|entry| entry.stage() == 0 && entry.path == path_bytes)
}

fn unmerged_stages_for_path(repo: &Repository, path: &str) -> Vec<u8> {
    let index = repo
        .index()
        .load(
            &grit_lib::index::IndexLoadOptions::default()
                .with_sparse(grit_lib::index::SparseIndexLoad::Preserve),
        )
        .expect("index");
    let path_bytes = path.as_bytes();
    let mut stages: Vec<u8> = index
        .entries()
        .iter()
        .filter(|entry| entry.stage() != 0 && entry.path == path_bytes)
        .map(|entry| entry.stage())
        .collect();
    stages.sort_unstable();
    stages
}

fn remote_refs_for_bare_heads(bare: &Repository) -> BTreeMap<String, String> {
    bare.references()
        .list("refs/heads/")
        .expect("list bare heads")
        .into_iter()
        .map(|(name, oid)| {
            let short = name
                .as_str()
                .strip_prefix("refs/heads/")
                .unwrap_or(name.as_str());
            (format!("refs/remotes/origin/{short}"), oid.to_hex())
        })
        .collect()
}

/// Expected `refs/remotes/origin/*` after fetch, derived only from the bare remote.
fn expected_fetch_remote_refs_from_bare(bare: &Repository) -> BTreeMap<String, String> {
    let mut expected = remote_refs_for_bare_heads(bare);
    let default_branch = bare
        .references()
        .symbolic("HEAD")
        .map(|t| t.map(String::from))
        .expect("read bare HEAD")
        .unwrap_or_else(|| "refs/heads/main".into());
    if let Some(short) = default_branch.strip_prefix("refs/heads/") {
        let tracking = format!("refs/remotes/origin/{short}");
        if let Some(oid) = expected.get(&tracking).cloned() {
            expected.insert("refs/remotes/origin/HEAD".into(), oid);
        }
    }
    expected
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
    /// Newest-first `main` commits: oid, subject line, change id.
    pub main_history: Vec<(ObjectId, String, ChangeId)>,
    /// The three `story.txt` commits recorded right after `c3` (must match [`Self::main_history`]).
    pub story_lineage: Vec<(ObjectId, String, ChangeId)>,
    /// Same three commits from the conformance `commit_path` helper outcomes (newest-first), for log cross-check.
    pub story_commits_from_outcomes: Vec<(ObjectId, String, ChangeId)>,
    /// Tip of `feature` after its single commit.
    pub feature_tip: ObjectId,
    /// Stable hunk ids for unstaged `story.txt` (worktree vs index).
    pub unstaged_hunk_ids: Vec<String>,
    /// Stable hunk ids for staged `staged-only.txt` (index vs `HEAD`).
    pub staged_hunk_ids: Vec<String>,
    /// Paths changed in the clean feature merge commit.
    pub committed_merge_paths: Vec<String>,
    /// Typed hunk ids for `feature.txt` in the clean merge commit diff.
    pub committed_merge_hunk_ids: Vec<String>,
    /// Structured hunk summaries for `feature.txt` in the clean merge commit diff.
    pub committed_feature_hunks: Vec<ConformanceHunkSummary>,
    pub staged_paths: Vec<String>,
    pub unstaged_paths: Vec<String>,
    pub untracked_paths: Vec<String>,
    pub untracked_paths_before_stash: Vec<String>,
    pub clean_merge_tip: ObjectId,
    /// Paths reported as unmerged during the conflicted merge.
    pub conflict_paths: Vec<String>,
    pub merge_in_progress_before_abort: bool,
    pub merge_conflicts_before_abort: Vec<String>,
    /// Typed conflict rows from status during the conflicted merge.
    pub conflict_details_before_abort: Vec<Conflict>,
    /// Unmerged index stages (1–3) for `shared.txt` before merge abort.
    pub shared_unmerged_stages_before_abort: Vec<u8>,
    pub shared_conflict_kind_before_abort: ConflictKind,
    pub index_conflicts_before_abort: Vec<Conflict>,
    pub index_conflicts_after_abort: Vec<Conflict>,
    pub merge_conflicts_after_abort: Vec<String>,
    pub conflict_details_after_abort: Vec<Conflict>,
    pub shared_has_conflict_markers: bool,
    pub main_head_at_conflict: ObjectId,
    pub head_after_merge_abort: ObjectId,
    pub merge_in_progress_after_abort: bool,
    pub shared_content_after_abort: String,
    pub pick_tip: ObjectId,
    pub revert_tip: ObjectId,
    /// Commit oid blamed for each line of `story.txt` (oldest line first).
    pub blame_commits: Vec<ObjectId>,
    /// Commit resolved from the short oid prefix of the second main commit.
    pub short_oid_resolved: ObjectId,
    /// Commit resolved from the change-id prefix of the tip main commit.
    pub change_id_resolved: ObjectId,
    pub note_body: Vec<u8>,
    pub stash_tip: ObjectId,
    pub stash_count_after_push: usize,
    pub stash_count_after_pop: usize,
    pub staged_only_content_after_stash_pop: String,
    pub untracked_content_after_stash_pop: String,
    pub story_content_after_stash_pop: String,
    pub staged_paths_after_stash_pop: Vec<String>,
    pub unstaged_paths_after_stash_pop: Vec<String>,
    pub untracked_paths_after_stash_pop: Vec<String>,
    pub untracked_file_after_stash_pop: bool,
    pub untracked_tracked_in_index_after_stash_pop: bool,
    pub pushed_main_tip: ObjectId,
    /// Every object reachable in the bare remote after push.
    pub bare_object_set: BTreeSet<String>,
    /// Objects in the backend-primary repo after fetch from that bare remote.
    pub backend_fetch_object_set: BTreeSet<String>,
    /// Objects in a files-primary repo after fetch from the same bare remote.
    pub files_fetch_object_set: BTreeSet<String>,
    pub expected_fetch_remote_refs: BTreeMap<String, String>,
    pub backend_fetch_refs: BTreeMap<String, String>,
    pub files_fetch_refs: BTreeMap<String, String>,
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
    let c1_out = commit_path(repo, "story.txt", "line1\n", "c1", 1_700_000_000);
    let c2_out = commit_path(repo, "story.txt", "line1\nline2\n", "c2", 1_700_000_001);
    let c3_out = commit_path(
        repo,
        "story.txt",
        "line1\nline2\nline3\n",
        "c3",
        1_700_000_002,
    );
    let c3 = head_oid(repo, "main");
    let story_lineage = log_main(repo);
    let mut story_commits_from_outcomes = vec![
        commit_history_row(&c1_out),
        commit_history_row(&c2_out),
        commit_history_row(&c3_out),
    ];
    story_commits_from_outcomes.reverse();

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

    let merge_out = repo
        .merge()
        .integrate_into_branch(
            &BranchIntegrateRequest {
                into_ref: "refs/heads/main",
                into_oid: main_before_merge,
                source: &MergeSource {
                    oid: feature_tip,
                    kind: MergeSourceKind::LocalBranch,
                },
                label: "feature",
            },
            &MergeOptions::default(),
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
    let pick_out = repo
        .merge()
        .start_sequence(
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
    let revert_out = repo
        .merge()
        .start_sequence(
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
    let conflict_out = repo
        .merge()
        .integrate_into_branch(
            &BranchIntegrateRequest {
                into_ref: "refs/heads/main",
                into_oid: main_conflict,
                source: &MergeSource {
                    oid: topic_conflict,
                    kind: MergeSourceKind::LocalBranch,
                },
                label: "conflict",
            },
            &MergeOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("conflict merge");
    let conflict_paths = match conflict_out {
        IntegrateBranchOutcome::Conflicts { paths, .. } => paths,
        other => panic!("expected conflicts, got {other:?}"),
    };
    let status_during_merge = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status during merge");
    let merge_in_progress_before_abort = status_during_merge.state.merge_in_progress;
    let merge_conflicts_before_abort = status_during_merge.conflicts.clone();
    let conflict_details_before_abort = status_during_merge.conflict_details.clone();
    let shared_unmerged_stages_before_abort = unmerged_stages_for_path(repo, "shared.txt");
    let stage_mask = shared_unmerged_stages_before_abort
        .iter()
        .fold(0u8, |mask, &stage| mask | (1u8 << (stage - 1)));
    let shared_conflict_kind_before_abort =
        conflict_kind_from_stagemask(stage_mask).expect("shared.txt unmerged during merge");
    let index_conflicts_before_abort = conflicts_from_index(
        &repo
            .index()
            .load(
                &grit_lib::index::IndexLoadOptions::default()
                    .with_sparse(grit_lib::index::SparseIndexLoad::Preserve),
            )
            .expect("index during merge"),
    );
    let shared_has_conflict_markers = fs::read_to_string(work_tree(repo).join("shared.txt"))
        .expect("shared")
        .contains("<<<<<<<");
    repo.merge().abort().expect("abort");
    let status_after_abort = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status after abort");
    let main_head_at_conflict = main_conflict;
    let head_after_merge_abort = head_oid(repo, "main");
    let merge_in_progress_after_abort = status_after_abort.state.merge_in_progress;
    let index_conflicts_after_abort = conflicts_from_index(
        &repo
            .index()
            .load(
                &grit_lib::index::IndexLoadOptions::default()
                    .with_sparse(grit_lib::index::SparseIndexLoad::Preserve),
            )
            .expect("index after abort"),
    );
    let merge_conflicts_after_abort = status_after_abort.conflicts.clone();
    let conflict_details_after_abort = status_after_abort.conflict_details.clone();
    let shared_content_after_abort =
        fs::read_to_string(work_tree(repo).join("shared.txt")).expect("shared after abort");

    let committed_merge_paths = repo
        .diff()
        .entries(
            &DiffTarget::Commit(clean_merge_tip),
            &Pathspec::entire_tree(PathspecGlobalFlags::default()),
            &DiffOptions::default(),
        )
        .expect("commit diff")
        .entries
        .iter()
        .map(|entry| entry.path().to_owned())
        .collect();
    let committed_feature_hunks =
        hunk_summaries(&commit_file_hunks(repo, clean_merge_tip, "feature.txt"));
    let committed_merge_hunk_ids = commit_file_hunk_hex_ids(repo, clean_merge_tip, "feature.txt");

    let blame = repo
        .diff()
        .blame("story.txt", &grit_lib::blame::BlameOptions::default())
        .expect("blame");
    let blame_commits: Vec<_> = blame.lines.iter().map(|l| l.commit).collect();

    let c2_oid = main_history[1].0;
    let short_prefix: String = c2_oid.to_hex().chars().take(7).collect();
    let short_oid_resolved = repo
        .revwalk()
        .resolve_commits(&[short_prefix])
        .expect("rev-parse short oid")[0];
    let change_id_prefix: String = main_history[0].2.as_str().chars().take(8).collect();
    let change_id_resolved =
        resolve_commit_by_change_id_prefix(repo, &change_id_prefix).expect("change-id prefix");

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

    const STAGED_ONLY_BEFORE: &str = "staged\n";
    const UNTRACKED_BEFORE: &str = "new\n";
    fs::write(work_tree(repo).join("staged-only.txt"), STAGED_ONLY_BEFORE).expect("write");
    repo.index()
        .stage(
            &StageOptions {
                pathspecs: vec!["staged-only.txt".into()],
                ..Default::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stage file");
    fs::write(work_tree(repo).join("untracked.txt"), UNTRACKED_BEFORE).expect("untracked");
    fs::write(
        work_tree(repo).join("story.txt"),
        "line1\nline2\nline3\nwip\n",
    )
    .expect("wip");
    let staged_hunk_ids = index_head_hunk_hex_ids(repo, "staged-only.txt");
    let unstaged_hunk_ids = unstaged_hunk_hex_ids(repo, "story.txt");
    let model = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status");
    let staged_paths: Vec<_> = model.staged.iter().map(|e| e.path().to_owned()).collect();
    let unstaged_paths: Vec<_> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
    let untracked_paths_before_stash = model.untracked.clone();
    let mut stash_push_opts =
        StashPushOptions::new(signature_at(1_700_000_060).parse().expect("sig"));
    stash_push_opts.include_untracked = true;
    let stash_out = repo
        .stash()
        .push(&stash_push_opts, &mut NullProgress, &CancelToken::never())
        .expect("stash push")
        .expect("stash created");
    let stash_tip = stash_out.oid;
    let stash_count_after_push = repo.stash().list().expect("stash list").len();

    repo.stash()
        .pop(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stash pop");
    let stash_count_after_pop = repo.stash().list().expect("stash list after pop").len();
    let staged_only_content_after_stash_pop =
        fs::read_to_string(work_tree(repo).join("staged-only.txt")).expect("staged-only after pop");
    let untracked_content_after_stash_pop =
        fs::read_to_string(work_tree(repo).join("untracked.txt")).expect("untracked after pop");
    let story_content_after_stash_pop =
        fs::read_to_string(work_tree(repo).join("story.txt")).expect("story after pop");
    let model_after_stash = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status after stash pop");
    let staged_paths_after_stash_pop: Vec<_> = model_after_stash
        .staged
        .iter()
        .map(|e| e.path().to_owned())
        .collect();
    let unstaged_paths_after_stash_pop: Vec<_> = model_after_stash
        .unstaged
        .iter()
        .map(|e| e.path().to_owned())
        .collect();
    let untracked_paths_after_stash_pop = model_after_stash.untracked.clone();
    let untracked_file_after_stash_pop = work_tree(repo).join("untracked.txt").is_file();
    let untracked_tracked_in_index_after_stash_pop = path_tracked_in_index(repo, "untracked.txt");

    let bare = root.join("remote.git");
    fs::create_dir_all(&bare).expect("bare");
    Repository::init(
        &bare,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main"),
    )
    .expect("init bare");
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

    let bare_repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open bare");
    let bare_object_set = object_hex_set(&bare_repo);
    let expected_fetch_remote_refs = expected_fetch_remote_refs_from_bare(&bare_repo);

    let bare_url = bare.to_str().expect("utf8");
    let origin = Remote::from_url(bare_url).expect("origin url");
    let mut fetch_opts = FetchOptions::default();
    fetch_opts.remote_name = Some("origin".into());
    fetch_opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".into()];

    let backend_dir = root.join("fetch-backend");
    fs::create_dir_all(&backend_dir).expect("mkdir backend fetch");
    let backend_fetch = fixture.init(&backend_dir, &suite_init_options());
    configure_local_identity(&backend_fetch);
    backend_fetch
        .remotes()
        .add("origin", bare_url)
        .expect("remote backend");
    backend_fetch.reload_config().expect("reload backend");
    backend_fetch
        .remotes()
        .fetch(
            &origin,
            fetch_opts.clone(),
            &mut NullProgress,
            &CancelToken::never(),
            None,
        )
        .expect("fetch backend");
    let backend_fetch_object_set = object_hex_set(&backend_fetch);
    let backend_fetch_refs = ref_map(&backend_fetch);

    let files_dir = root.join("fetch-files");
    fs::create_dir_all(&files_dir).expect("mkdir files fetch");
    let files_fetch = init_files_primary_repo(&files_dir);
    configure_local_identity(&files_fetch);
    files_fetch
        .remotes()
        .add("origin", bare_url)
        .expect("remote files");
    files_fetch.reload_config().expect("reload files");
    files_fetch
        .remotes()
        .fetch(
            &origin,
            fetch_opts,
            &mut NullProgress,
            &CancelToken::never(),
            None,
        )
        .expect("fetch files");
    let files_fetch_object_set = object_hex_set(&files_fetch);
    let files_fetch_refs = ref_map(&files_fetch);

    let fsck = repo
        .maintenance()
        .fsck(
            &FsckOptions::full_repository(true),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("fsck");
    let fsck_clean = fsck.issues.is_empty();

    RepositorySnapshot {
        main_history,
        story_lineage,
        story_commits_from_outcomes,
        feature_tip,
        unstaged_hunk_ids,
        staged_hunk_ids,
        committed_merge_paths,
        committed_merge_hunk_ids,
        committed_feature_hunks,
        staged_paths,
        unstaged_paths,
        untracked_paths: model.untracked,
        untracked_paths_before_stash,
        clean_merge_tip,
        conflict_paths,
        merge_in_progress_before_abort,
        merge_conflicts_before_abort,
        conflict_details_before_abort,
        shared_unmerged_stages_before_abort,
        shared_conflict_kind_before_abort,
        index_conflicts_before_abort,
        index_conflicts_after_abort,
        merge_conflicts_after_abort,
        conflict_details_after_abort,
        shared_has_conflict_markers,
        main_head_at_conflict,
        head_after_merge_abort,
        merge_in_progress_after_abort,
        shared_content_after_abort,
        pick_tip,
        revert_tip,
        blame_commits,
        short_oid_resolved,
        change_id_resolved,
        note_body,
        stash_tip,
        stash_count_after_push,
        stash_count_after_pop,
        staged_only_content_after_stash_pop,
        untracked_content_after_stash_pop,
        story_content_after_stash_pop,
        staged_paths_after_stash_pop,
        unstaged_paths_after_stash_pop,
        untracked_paths_after_stash_pop,
        untracked_file_after_stash_pop,
        untracked_tracked_in_index_after_stash_pop,
        pushed_main_tip,
        bare_object_set,
        backend_fetch_object_set,
        files_fetch_object_set,
        expected_fetch_remote_refs,
        backend_fetch_refs,
        files_fetch_refs,
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
    assert_eq!(snap.story_lineage, snap.story_commits_from_outcomes);
    assert_eq!(snap.story_lineage.len(), 3);
    let expected_subjects = ["c3", "c2", "c1"];
    for (i, subject) in expected_subjects.iter().enumerate() {
        let (oid, subj, change_id) = &snap.story_lineage[i];
        assert_eq!(subj.as_str(), *subject);
        assert_ne!(*oid, ObjectId::null(HashAlgo::Sha1));
        assert!(!change_id.as_str().is_empty());
        let (out_oid, out_subj, out_change_id) = &snap.story_commits_from_outcomes[i];
        assert_eq!(*oid, *out_oid);
        assert_eq!(subj, out_subj);
        assert_eq!(change_id, out_change_id);
    }
    let story_tip = snap.story_lineage[0].0;
    let main_pos = snap
        .main_history
        .iter()
        .position(|(oid, _, _)| *oid == story_tip)
        .expect("main log includes story tip");
    assert_eq!(snap.main_history[main_pos], snap.story_lineage[0]);
    assert_eq!(snap.main_history[main_pos + 1], snap.story_lineage[1]);
    assert_eq!(snap.main_history[main_pos + 2], snap.story_lineage[2]);
    let change_ids: BTreeSet<_> = snap
        .story_lineage
        .iter()
        .map(|(_, _, id)| id.as_str())
        .collect();
    assert_eq!(change_ids.len(), 3);
    assert_ne!(snap.feature_tip, snap.story_lineage[0].0);
}

fn case_diff_and_status(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert!(!snap.unstaged_hunk_ids.is_empty());
    assert!(!snap.staged_hunk_ids.is_empty());
    assert!(!snap.committed_merge_hunk_ids.is_empty());
    assert_eq!(
        snap.committed_feature_hunks,
        vec![ConformanceHunkSummary {
            old_start: 0,
            new_start: 1,
            old_lines: 0,
            new_lines: 1,
            removed: vec![],
            added: vec!["feature".into()],
        }]
    );
    assert!(snap
        .committed_merge_paths
        .iter()
        .any(|p| p == "feature.txt"));
    assert!(snap.staged_paths.iter().any(|p| p == "staged-only.txt"));
    assert!(snap.unstaged_paths.iter().any(|p| p == "story.txt"));
    assert!(snap.untracked_paths.iter().any(|p| p == "untracked.txt"));
}

fn case_merge_clean(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_ne!(snap.clean_merge_tip, ObjectId::null(HashAlgo::Sha1));
}

fn case_merge_conflict_abort(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert!(snap.conflict_paths.iter().any(|p| p == "shared.txt"));
    assert!(snap.merge_in_progress_before_abort);
    assert!(snap
        .merge_conflicts_before_abort
        .iter()
        .any(|p| p == "shared.txt"));
    assert_eq!(snap.shared_unmerged_stages_before_abort, vec![2, 3]);
    assert_eq!(
        snap.shared_conflict_kind_before_abort,
        ConflictKind::BothAdded
    );
    assert_eq!(
        snap.conflict_details_before_abort,
        vec![Conflict {
            path: "shared.txt".into(),
            kind: ConflictKind::BothAdded,
        }]
    );
    assert_eq!(
        snap.index_conflicts_before_abort,
        vec![Conflict {
            path: "shared.txt".into(),
            kind: ConflictKind::BothAdded,
        }]
    );
    assert!(snap.shared_has_conflict_markers);
    assert!(snap.merge_conflicts_after_abort.is_empty());
    assert!(snap.conflict_details_after_abort.is_empty());
    assert!(snap.index_conflicts_after_abort.is_empty());
    assert!(!snap.merge_in_progress_after_abort);
    assert_eq!(snap.shared_content_after_abort, "main side\n");
    assert_eq!(snap.head_after_merge_abort, snap.main_head_at_conflict);
}

fn case_pick_and_revert(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_ne!(snap.pick_tip, snap.revert_tip);
}

fn case_blame(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_eq!(snap.blame_commits.len(), 3);
    let expected: Vec<_> = snap
        .main_history
        .iter()
        .rev()
        .map(|(oid, _, _)| *oid)
        .collect();
    assert_eq!(snap.blame_commits, expected);
}

fn case_rev_parse(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_eq!(snap.short_oid_resolved, snap.main_history[1].0);
    assert_eq!(snap.change_id_resolved, snap.main_history[0].0);
}

fn case_notes(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_eq!(snap.note_body, b"conformance note\n");
}

fn case_stash(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    assert_ne!(snap.stash_tip, ObjectId::null(HashAlgo::Sha1));
    assert_eq!(snap.stash_count_after_push, 1);
    assert_eq!(snap.stash_count_after_pop, 0);
    assert!(snap
        .staged_paths_after_stash_pop
        .iter()
        .any(|p| p == "staged-only.txt"));
    assert!(snap
        .unstaged_paths_after_stash_pop
        .iter()
        .any(|p| p == "story.txt"));
    assert_eq!(
        snap.untracked_paths_before_stash,
        ["untracked.txt".to_owned()]
    );
    assert!(snap.untracked_file_after_stash_pop);
    assert!(!snap.untracked_tracked_in_index_after_stash_pop);
    assert_eq!(
        snap.untracked_paths_after_stash_pop,
        ["untracked.txt".to_owned()]
    );
    assert_eq!(
        snap.staged_paths_after_stash_pop,
        ["staged-only.txt".to_owned()]
    );
    assert_eq!(
        snap.unstaged_paths_after_stash_pop,
        ["story.txt".to_owned()]
    );
    assert_eq!(snap.staged_only_content_after_stash_pop, "staged\n");
    assert_eq!(snap.untracked_content_after_stash_pop, "new\n");
    assert_eq!(
        snap.story_content_after_stash_pop,
        "line1\nline2\nline3\nwip\n"
    );
}

fn case_fetch(fixture: &impl BackendFixture) {
    let snap = build_snapshot(fixture);
    let backend_remotes = remote_tracking_refs(&snap.backend_fetch_refs);
    let files_remotes = remote_tracking_refs(&snap.files_fetch_refs);
    assert_eq!(backend_remotes, snap.expected_fetch_remote_refs);
    assert_eq!(files_remotes, snap.expected_fetch_remote_refs);
    assert_eq!(backend_remotes, files_remotes);
    assert_eq!(snap.backend_fetch_object_set, snap.bare_object_set);
    assert_eq!(snap.files_fetch_object_set, snap.bare_object_set);
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

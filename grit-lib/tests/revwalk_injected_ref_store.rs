//! Rev-walk queries, `:/text` searches and linked worktree add/list/remove read and write
//! refs through [`Repository::refs`] (issue #1889): an injected [`MemoryRefStore`] gives the
//! same answers as a files repository with the same history, and the files variant agrees
//! with the system `git`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::branch::{create_branch, CreateBranchOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::tag::{create_tag, CreateTagRequest};
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::{MemoryRefStore, RawRef, RefStore};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_lib::revwalk::queries::{RefSelection, RevWalkListOptions};
use grit_lib::revwalk::rev_list::{collect_revision_specs_from_stdin_lines, tag_targets};
use grit_lib::state::HeadState;
use grit_lib::worktree::{AddLinkedWorktreeOptions, WorktreeBranch, WorktreeEntry};

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";
const USER_CONFIG: &str = "[user]\n\tname = T\n\temail = t@e.x\n[grit]\n\tchangeIds = false\n";

fn options() -> RepositoryOptions {
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    RepositoryOptions::with_environment(env)
}

/// Repository with an in-memory object store and ref store whose git directory holds only
/// `config` (and later the index).
fn memory_repo(git_dir: &Path, work_tree: &Path) -> (Repository, Arc<MemoryRefStore>) {
    fs::create_dir_all(git_dir).expect("git dir");
    fs::create_dir_all(work_tree).expect("work tree");
    fs::write(git_dir.join("config"), USER_CONFIG).expect("config");
    let store = Arc::new(MemoryRefStore::new());
    let repo = RepositoryBuilder::new(options())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(store.clone())
        .initial_branch("main")
        .open(git_dir, Some(work_tree))
        .expect("open in-memory repository");
    (repo, store)
}

fn files_repo(work_tree: &Path) -> Repository {
    Repository::init(work_tree, &InitOptions::new().with_initial_branch("main")).expect("init");
    let config = work_tree.join(".git/config");
    let mut text = fs::read_to_string(&config).expect("config");
    text.push_str(USER_CONFIG);
    fs::write(&config, text).expect("write config");
    Repository::open(
        &work_tree.join(".git"),
        &options().with_work_tree(work_tree),
    )
    .expect("open")
}

fn commit_all(repo: &Repository, message: &str) -> ObjectId {
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    commit_with_scope(
        repo,
        &CommitRequest {
            message: message.into(),
            author: Some(IDENT_TS.parse().expect("signature")),
            committer: Some(IDENT_TS.parse().expect("signature")),
            ..Default::default()
        },
        CommitScope::Staged,
        &mut NullProgress,
    )
    .expect("commit")
    .oid
}

/// Everything [`battery`] observed, compared across ref backends.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    main: ObjectId,
    hello: ObjectId,
    second: ObjectId,
    count_all: usize,
    count_branches: usize,
    count_tags: usize,
    count_tags_glob: usize,
    tag_targets: Vec<ObjectId>,
    stdin_all_negated: Vec<String>,
    stdin_branches: Vec<String>,
    stdin_tags_glob: Vec<String>,
    linked_head: HeadState,
    linked_listed_head: HeadState,
    linked_branch_tip: ObjectId,
}

fn count(repo: &Repository, refs: Vec<RefSelection>) -> usize {
    let options = RevWalkListOptions {
        refs,
        ..RevWalkListOptions::default()
    };
    repo.rev_walk().count(&[], &options).expect("count")
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Two commits on `main`, a `side` branch at the first, a tag on the first and a linked
/// worktree on a new branch, queried through rev-walk and worktree APIs.
fn battery(repo: &Repository, wt: &Path, linked: &Path) -> Observed {
    fs::write(wt.join("f"), "x\n").expect("write");
    let first = commit_all(repo, "hello world");
    create_branch(repo, &CreateBranchOptions::at_head("side")).expect("side");
    create_tag(
        repo,
        &CreateTagRequest {
            name: "v1".into(),
            target: first,
            message: None,
            tagger: None,
            sign: None,
            signing_key: None,
            tree_sha256: None,
            force: false,
            dry_run: false,
        },
    )
    .expect("tag");
    fs::write(wt.join("g"), "y\n").expect("write");
    let second = commit_all(repo, "second change");

    let walk = repo.rev_walk();
    let main = walk.resolve("main").expect("resolve main");
    let hello = walk.resolve(":/hello").expect("resolve :/hello");
    assert_eq!(hello, first);
    assert_eq!(walk.resolve(":/second").expect(":/second"), second);

    let stdin = |lines: &[&str]| {
        collect_revision_specs_from_stdin_lines(repo, &[], lines).expect("stdin specs")
    };
    let (_, stdin_all_negated, all_flag, _) = stdin(&["--not", "--all"]);
    assert!(!all_flag);
    let (stdin_branches, _, _, _) = stdin(&["--branches"]);
    let (stdin_tags_glob, _, _, _) = stdin(&["--tags=v*"]);
    let mut tags: Vec<ObjectId> = tag_targets(repo).expect("tags").into_iter().collect();
    tags.sort();

    let added: WorktreeEntry = repo
        .worktrees()
        .add(&AddLinkedWorktreeOptions {
            path: linked.to_path_buf(),
            branch: Some(WorktreeBranch::Create("linked".into())),
            start_point: Some("side".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        })
        .expect("worktrees().add");
    assert_eq!(
        fs::read_to_string(linked.join("f")).expect("checked out f"),
        "x\n"
    );
    assert!(
        !linked.join("g").exists(),
        "linked worktree is at `side`, which predates g"
    );
    let listed = repo.worktrees().list().expect("list");
    assert_eq!(listed.len(), 2);
    let linked_row = listed
        .iter()
        .find(|e| e.admin_dir == added.admin_dir)
        .expect("linked worktree listed");

    let observed = Observed {
        main,
        hello,
        second,
        count_all: count(repo, vec![RefSelection::All]),
        count_branches: count(repo, vec![RefSelection::Branches(None)]),
        count_tags: count(repo, vec![RefSelection::Tags(None)]),
        count_tags_glob: count(repo, vec![RefSelection::Tags(Some("v*".into()))]),
        tag_targets: tags,
        stdin_all_negated: sorted(stdin_all_negated),
        stdin_branches: sorted(stdin_branches),
        stdin_tags_glob,
        linked_head: added.head.clone(),
        linked_listed_head: linked_row.head.clone(),
        linked_branch_tip: walk.resolve("linked").expect("linked branch"),
    };

    repo.worktrees()
        .remove(linked, false)
        .expect("remove a clean linked worktree");
    assert!(!linked.exists());
    assert_eq!(repo.worktrees().list().expect("list").len(), 1);
    observed
}

fn expected_head(first: ObjectId) -> HeadState {
    HeadState::Branch {
        refname: "refs/heads/linked".into(),
        short_name: "linked".into(),
        oid: Some(first),
    }
}

fn assert_battery_shape(observed: &Observed) {
    assert_eq!(observed.main, observed.second);
    assert_eq!(observed.count_all, 2);
    assert_eq!(observed.count_branches, 2);
    assert_eq!(observed.count_tags, 1);
    assert_eq!(observed.count_tags_glob, 1);
    assert_eq!(observed.tag_targets, vec![observed.hello]);
    assert_eq!(observed.stdin_tags_glob, vec![observed.hello.to_hex()]);
    assert_eq!(observed.linked_head, expected_head(observed.hello));
    assert_eq!(observed.linked_listed_head, expected_head(observed.hello));
    assert_eq!(observed.linked_branch_tip, observed.hello);
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn run_files() -> Observed {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path().join("wt");
    fs::create_dir_all(&wt).expect("wt");
    let repo = files_repo(&wt);
    let observed = battery(&repo, &wt, &dir.path().join("linked"));
    git(&wt, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(
        git(&wt, &["rev-list", "--all", "--count"]).trim(),
        observed.count_all.to_string()
    );
    assert_eq!(
        git(&wt, &["rev-parse", ":/hello"]).trim(),
        observed.hello.to_hex()
    );
    observed
}

fn assert_nothing_on_disk(git_dir: &Path) {
    assert!(!git_dir.join("refs").exists());
    assert!(!git_dir.join("logs").exists());
    assert!(!git_dir.join("packed-refs").exists());
    assert!(!git_dir.join("HEAD").exists());
}

fn assert_store(store: &MemoryRefStore, observed: &Observed) {
    assert_eq!(
        store.read_raw("refs/heads/linked").expect("linked"),
        Some(RawRef::Direct(observed.hello))
    );
    assert_eq!(
        store.read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".into())),
        "adding a linked worktree must not move the main worktree's HEAD"
    );
}

#[test]
fn injected_store_with_git_dir_in_work_tree_matches_files_repository() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path().join("wt");
    let git_dir: PathBuf = wt.join(".git");
    let (repo, store) = memory_repo(&git_dir, &wt);
    let observed = battery(&repo, &wt, &dir.path().join("linked"));
    assert_battery_shape(&observed);
    assert_store(&store, &observed);
    assert_nothing_on_disk(&git_dir);
    assert_eq!(observed, run_files());
}

#[test]
fn injected_store_with_git_dir_outside_work_tree_matches_files_repository() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path().join("wt");
    let git_dir = dir.path().join("gitdir");
    let (repo, store) = memory_repo(&git_dir, &wt);
    assert!(!repo.is_bare());
    let observed = battery(&repo, &wt, &dir.path().join("linked"));
    assert_battery_shape(&observed);
    assert_store(&store, &observed);
    assert_nothing_on_disk(&git_dir);
    assert_eq!(observed, run_files());
}

#[test]
fn files_repository_battery_is_git_compatible() {
    assert_battery_shape(&run_files());
}

#[test]
fn mid_component_ref_globs_match_packed_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path();
    let repo = files_repo(wt);
    fs::write(wt.join("f"), "x\n").expect("write");
    let tip = commit_all(&repo, "first");
    for name in ["refs/tags/v1", "refs/tags/w1", "refs/heads/b05-x"] {
        git(wt, &["update-ref", name, &tip.to_hex()]);
    }
    git(wt, &["pack-refs", "--all"]);
    let repo = Repository::open(&wt.join(".git"), &options().with_work_tree(wt)).expect("open");

    let (tags, _, _, _) =
        collect_revision_specs_from_stdin_lines(&repo, &[], &["--tags=v*"]).expect("stdin");
    assert_eq!(tags, vec![tip.to_hex()]);
    let (branches, _, _, _) =
        collect_revision_specs_from_stdin_lines(&repo, &[], &["--branches=b05*"]).expect("stdin");
    assert_eq!(branches, vec![tip.to_hex()]);
    assert_eq!(count(&repo, vec![RefSelection::Tags(Some("v*".into()))]), 1);
    assert_eq!(
        git(wt, &["rev-list", "--tags=v*", "--count"]).trim(),
        "1",
        "git agrees the glob matches"
    );
}

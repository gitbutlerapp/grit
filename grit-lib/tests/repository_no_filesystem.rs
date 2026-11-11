//! A bare repository whose object store, ref store and config are all injected works
//! without a git directory on disk.
//!
//! The git directory path is never created: building commits, ref transactions, log,
//! diff, tree merges, blame, rev-parse, notes, bundle creation and a local fetch all run
//! against [`MemoryStore`] and [`MemoryRefStore`], and nothing appears under the parent
//! directory.

use grit_lib::objects::FileMode;
use std::path::Path;
use std::sync::Arc;

use grit_lib::blame::BlameOptions;
use grit_lib::bundle::BundleSpec;
use grit_lib::config::environment::{ConfigOverride, Environment, RepositoryOptions};
use grit_lib::diff::{DiffStatus, TreeDiffOptions};
use grit_lib::ident::Signature;
use grit_lib::merge::MergePredictionOptions;
use grit_lib::notes::NotesRef;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::{parse_tree, HashAlgo, NewCommit, ObjectId, ObjectKind, TreeEntry};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::store::{MemoryRefStore, RefName};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::FetchOptions;
use grit_lib::remote::Remote;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::{NoteAddOutcome, NotesWriteOptions};
use grit_lib::revwalk::history::{ApplyOptions, RefScope, RewriteOptions};
use grit_lib::revwalk::rev_list::RevListOptions;
use grit_lib::revwalk::ResolveOptions;
use grit_test_support::write_merge_tree;
use time::OffsetDateTime;

fn signature(name: &str, seconds: i64) -> Signature {
    let when = OffsetDateTime::from_unix_timestamp(seconds).expect("timestamp");
    Signature::new(name, &format!("{}@example.com", name.to_lowercase()), when).expect("signature")
}

fn in_memory_environment() -> Environment {
    let mut env = Environment::empty();
    env.git_config_nosystem = Some("1".to_owned());
    for (key, value) in [
        ("user.name", "Injected Committer"),
        ("user.email", "injected@example.com"),
        ("core.logAllRefUpdates", "true"),
    ] {
        env.push_config_override(
            &ConfigOverride::new(key, Some(value.to_owned())).expect("config override"),
        );
    }
    env
}

fn open_injected(git_dir: &Path) -> (Repository, Arc<MemoryStore>, Arc<MemoryRefStore>) {
    let objects = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let refs = Arc::new(MemoryRefStore::new());
    let repo = Repository::open(
        git_dir,
        &RepositoryOptions::with_environment(in_memory_environment())
            .with_object_store(objects.clone())
            .with_ref_store(refs.clone())
            .with_initial_branch("main"),
    )
    .expect("open fully injected repository");
    (repo, objects, refs)
}

fn write_file_tree(repo: &Repository, files: &[(&str, &str)]) -> ObjectId {
    let entries = files
        .iter()
        .map(|(name, body)| {
            TreeEntry::new(
                name.as_bytes().to_vec(),
                FileMode::REGULAR,
                repo.objects()
                    .write(ObjectKind::Blob, body.as_bytes())
                    .expect("blob"),
            )
        })
        .collect::<Vec<_>>();
    repo.objects().write_tree(entries).expect("tree")
}

fn write_commit(
    repo: &Repository,
    files: &[(&str, &str)],
    parents: Vec<ObjectId>,
    message: &str,
    seconds: i64,
) -> ObjectId {
    let tree = write_file_tree(repo, files);
    let who = signature("Ada", seconds);
    repo.objects()
        .write_commit(&NewCommit::new(tree, who.clone(), who, message).with_parents(parents))
        .expect("commit")
}

fn branch(name: &str) -> RefName {
    RefName::try_from(format!("refs/heads/{name}").as_str()).expect("ref name")
}

/// Every path below `dir`, relative to it, so a failure names what was created.
fn entries_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read_dir").flatten() {
            let path = entry.path();
            out.push(
                path.strip_prefix(dir)
                    .expect("under dir")
                    .display()
                    .to_string(),
            );
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}

fn assert_untouched(scratch: &Path, step: &str) {
    assert_eq!(
        entries_under(scratch),
        Vec::<String>::new(),
        "nothing may be created under the scratch directory ({step})"
    );
}

/// A files-backed bare repository with one commit on `main`, written through grit-lib.
fn files_upstream(dir: &Path) -> (std::path::PathBuf, ObjectId) {
    let git_dir = dir.join("upstream.git");
    Repository::init(
        &git_dir,
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main"),
    )
    .expect("init upstream");
    let upstream = Repository::open(
        &git_dir,
        &RepositoryOptions::with_environment(in_memory_environment()),
    )
    .expect("open upstream");
    let tip = write_commit(
        &upstream,
        &[("upstream.txt", "from a files repository\n")],
        Vec::new(),
        "upstream root",
        1_700_000_500,
    );
    upstream
        .references()
        .update(&branch("main"), &tip)
        .expect("upstream main");
    (git_dir, tip)
}

#[test]
fn bare_repository_with_every_store_injected_never_touches_the_filesystem() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let git_dir = scratch.path().join("never-created.git");
    let (repo, objects, _refs) = open_injected(&git_dir);
    assert!(repo.is_bare());
    assert_eq!(
        repo.config()
            .snapshot()
            .expect("config")
            .get("user.name")
            .expect("user.name")
            .as_deref(),
        Some("Injected Committer"),
        "config comes from the environment, not from <git_dir>/config"
    );

    let base = write_commit(
        &repo,
        &[("a.txt", "one\ntwo\nthree\n"), ("b.txt", "b\n")],
        Vec::new(),
        "base",
        1_700_000_000,
    );
    let ours = write_commit(
        &repo,
        &[("a.txt", "ONE\ntwo\nthree\n"), ("b.txt", "b\n")],
        vec![base],
        "ours",
        1_700_000_100,
    );
    let theirs = write_commit(
        &repo,
        &[
            ("a.txt", "one\ntwo\nTHREE\n"),
            ("b.txt", "b\n"),
            ("c.txt", "c\n"),
        ],
        vec![base],
        "theirs",
        1_700_000_200,
    );
    assert!(objects.contains(&ours).expect("contains"));

    repo.references()
        .update(&branch("main"), &ours)
        .expect("update main through HEAD's branch");
    repo.references()
        .update(&branch("topic"), &theirs)
        .expect("create topic");
    assert_eq!(
        repo.references()
            .resolve(&RefName::new("HEAD").expect("valid ref name"))
            .expect("HEAD"),
        ours
    );
    let main_log = repo.references().reflog(&branch("main")).expect("reflog");
    assert_eq!(main_log.last().map(|entry| entry.new_oid), Some(ours));

    assert_untouched(scratch.path(), "before rev-parse");

    // rev-parse
    assert_eq!(
        repo.revwalk()
            .resolve("HEAD", &ResolveOptions::default())
            .expect("HEAD"),
        ours
    );
    assert_eq!(
        repo.revwalk()
            .resolve("main~1", &ResolveOptions::default())
            .expect("main~1"),
        base
    );
    assert_eq!(
        repo.revwalk()
            .resolve("topic^", &ResolveOptions::default())
            .expect("topic^"),
        base
    );
    let short = &theirs.to_hex()[..10];
    assert_eq!(
        repo.revwalk()
            .resolve(short, &ResolveOptions::default())
            .expect("short id"),
        theirs
    );

    assert_untouched(scratch.path(), "before log");

    // log
    let walked = repo
        .revwalk()
        .walk(
            "main",
            &RevListOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("walk");
    assert_eq!(walked.commits, vec![ours, base]);
    let subjects = walked
        .commits
        .iter()
        .map(|oid| repo.objects().read_commit(oid).expect("commit").message)
        .collect::<Vec<_>>();
    assert_eq!(subjects, vec!["ours\n", "base\n"]);
    let branches = repo
        .references()
        .list("refs/heads/")
        .expect("list branches");
    assert_eq!(branches.len(), 2);

    assert_untouched(scratch.path(), "before diff of two commits");

    // diff of two commits
    let old_tree = repo.objects().commit_tree(&ours).expect("ours tree");
    let new_tree = repo.objects().commit_tree(&theirs).expect("theirs tree");
    let changes = repo
        .diff()
        .tree_to_tree(
            Some(&old_tree),
            Some(&new_tree),
            &TreeDiffOptions::default(),
        )
        .expect("diff");
    let summary = changes
        .entries
        .iter()
        .map(|entry| (entry.path().to_owned(), entry.status))
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        vec![
            ("a.txt".to_owned(), DiffStatus::Modified),
            ("c.txt".to_owned(), DiffStatus::Added),
        ]
    );

    assert_untouched(scratch.path(), "before tree merge, no work tree");

    // tree merge, no work tree
    let merged =
        write_merge_tree(&repo, ours, theirs, &MergePredictionOptions::default()).expect("merge");
    assert!(merged.is_clean(), "{:?}", merged.conflicts);
    let merged_tree =
        parse_tree(&repo.objects().read(&merged.tree).expect("merged tree").data).expect("parse");
    let a_blob = merged_tree
        .iter()
        .find(|entry| entry.name == b"a.txt")
        .expect("a.txt")
        .oid;
    assert_eq!(
        repo.objects().read(&a_blob).expect("merged blob").data,
        b"ONE\ntwo\nTHREE\n"
    );
    let who = signature("Ada", 1_700_000_300);
    let merge_commit = repo
        .objects()
        .write_commit(
            &NewCommit::new(merged.tree, who.clone(), who, "merge topic")
                .with_parents(vec![ours, theirs]),
        )
        .expect("merge commit");
    repo.references()
        .compare_and_swap(&branch("main"), &ours, &merge_commit)
        .expect("advance main");

    assert_untouched(scratch.path(), "before blame");

    // blame
    let blame = repo
        .diff()
        .blame(
            "a.txt",
            &BlameOptions {
                start: Some(merge_commit),
                ..BlameOptions::default()
            },
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("blame");
    let blamed = blame
        .lines
        .iter()
        .map(|line| line.commit)
        .collect::<Vec<_>>();
    assert_eq!(blamed, vec![ours, base, theirs]);

    assert_untouched(scratch.path(), "before notes");

    // notes
    let notes_ref = NotesRef::commits();
    let outcome = repo
        .notes()
        .add(
            &notes_ref,
            &merge_commit,
            b"reviewed\n",
            &NotesWriteOptions {
                message: "add note".to_owned(),
                committer: signature("Ada", 1_700_000_400),
                author: None,
            },
            false,
        )
        .expect("add note");
    assert_eq!(outcome, NoteAddOutcome::Added);
    assert_eq!(
        repo.notes()
            .read(&notes_ref, &merge_commit)
            .expect("read note")
            .as_deref(),
        Some(&b"reviewed\n"[..])
    );

    assert_untouched(scratch.path(), "before bundle into memory");

    // bundle into memory
    let mut bundle = Vec::new();
    repo.bundles()
        .write_to(
            &BundleSpec {
                include: vec![(merge_commit, "refs/heads/main".into())],
                ..BundleSpec::default()
            },
            &mut bundle,
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("bundle");
    assert!(bundle.starts_with(b"# v2 git bundle\n"));
    assert!(bundle.windows(4).any(|window| window == b"PACK"));

    assert_untouched(scratch.path(), "before history reword");

    // History edits run post-rewrite hooks; a missing hooks directory means no hooks.
    assert!(repo.hooks().list(None).expect("hooks").items.is_empty());
    let committer: Signature = "Ada <ada@example.com> 1700000600 +0000"
        .parse()
        .expect("committer");
    let plan = repo
        .history()
        .reword(
            merge_commit,
            &grit_lib::revwalk::history::RewordOptions::default()
                .with_message("merge topic (reworded)".to_owned()),
            &RewriteOptions::new(committer.clone())
                .with_scope(RefScope::Head)
                .with_sign(false),
        )
        .expect("plan reword");
    let applied = repo
        .history()
        .apply(
            &plan,
            &ApplyOptions::new(committer, "history reword".to_owned()),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("apply reword");
    assert_eq!(
        repo.revwalk()
            .resolve("main", &ResolveOptions::default())
            .expect("main"),
        applied.head
    );
    assert_eq!(
        repo.objects()
            .read_commit(&applied.head)
            .expect("reworded")
            .parents,
        vec![ours, theirs]
    );

    assert_untouched(scratch.path(), "before local fetch from a files repository");

    // local fetch from a files repository
    let upstream_dir = tempfile::tempdir().expect("upstream tempdir");
    let (upstream_git_dir, upstream_tip) = files_upstream(upstream_dir.path());
    let remote = Remote::from_url(upstream_git_dir.to_str().expect("utf-8 path")).expect("remote");
    let mut fetch = FetchOptions::default();
    fetch.refspecs = vec!["+refs/heads/*:refs/remotes/upstream/*".to_owned()];
    let fetched = repo
        .remotes()
        .fetch(&remote, fetch, &mut NoProgress, &CancelToken::never(), None)
        .expect("fetch");
    assert_eq!(fetched.branch_count(), 1);
    assert!(objects.contains(&upstream_tip).expect("contains fetched"));
    assert_eq!(
        repo.revwalk()
            .resolve("refs/remotes/upstream/main", &ResolveOptions::default())
            .expect("tracking ref"),
        upstream_tip
    );

    assert_untouched(scratch.path(), "after local fetch");
    assert!(!git_dir.exists(), "{} was created", git_dir.display());
}

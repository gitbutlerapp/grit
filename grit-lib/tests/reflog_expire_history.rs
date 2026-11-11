//! Reflog expiry on a repository with real history (issue #2192), checked byte-for-byte
//! against `git reflog expire`.
//!
//! The history has wide trees and a long chain of merge diamonds, and several reflog entries
//! are older than the unreachable cutoff. Expiry must only walk commits once per ref, so it
//! finishes well inside the time limit; a per-entry walk of full history and trees that
//! re-walks every diamond would not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;
use grit_lib::objects::FileMode;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::objects::{serialize_tree, ObjectId, ObjectKind, TreeEntry};
use grit_lib::refs::reflog::{
    expire_reflog_git, load_gc_reflog_expire_config, ReflogEntry, ReflogExpireActionKind,
    ReflogExpireParams,
};
use grit_lib::refs::store::RefNameRef;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::ExpireOptions;
use grit_lib::test_support::refs::reflog_file_path;

use support::{
    assert_reflog_tree_matches, copy_worktree, git, git_fsck_strict, git_version_at_least,
    reflog_identity,
};

const NOW: i64 = 1_700_000_000;
const OLD: i64 = 1_500_000_000;
const UNREACHABLE_CUTOFF: i64 = 1_600_000_000;
const RECENT: i64 = 1_690_000_000;
/// Commit dates are `1_400_000_000 + n` for the n-th generated commit.
const MID_HISTORY: i64 = 1_400_000_060;
const DIRS: usize = 8;
const FILES_PER_DIR: usize = 32;
const LINEAR_COMMITS: usize = 40;
const DIAMONDS: usize = 30;
const TIME_LIMIT: Duration = Duration::from_secs(60);

fn open_repo(worktree: &Path) -> Repository {
    Repository::open(
        &worktree.join(".git"),
        &RepositoryOptions::empty().with_work_tree(worktree),
    )
    .expect("Repository::open")
}

/// Builds wide trees and commits directly in the object database.
struct HistoryBuilder<'a> {
    repo: &'a Repository,
    files: Vec<Vec<ObjectId>>,
    serial: usize,
}

impl<'a> HistoryBuilder<'a> {
    fn new(repo: &'a Repository) -> Self {
        let files = (0..DIRS)
            .map(|d| {
                (0..FILES_PER_DIR)
                    .map(|f| {
                        repo.odb()
                            .write(ObjectKind::Blob, format!("dir {d} file {f}\n").as_bytes())
                            .expect("write blob")
                    })
                    .collect()
            })
            .collect();
        Self {
            repo,
            files,
            serial: 0,
        }
    }

    fn write_tree(&self, files: &[Vec<ObjectId>]) -> ObjectId {
        let odb = self.repo.odb();
        let dirs = files
            .iter()
            .enumerate()
            .map(|(d, blobs)| {
                let entries = blobs
                    .iter()
                    .enumerate()
                    .map(|(f, oid)| {
                        TreeEntry::new(format!("f{f:03}").into_bytes(), FileMode::REGULAR, *oid)
                    })
                    .collect::<Vec<_>>();
                TreeEntry::new(
                    format!("d{d}").into_bytes(),
                    FileMode::TREE,
                    odb.write(ObjectKind::Tree, &serialize_tree(&entries))
                        .expect("write subtree"),
                )
            })
            .collect::<Vec<_>>();
        odb.write(ObjectKind::Tree, &serialize_tree(&dirs))
            .expect("write root tree")
    }

    /// A commit on `parents` that changes one file of the shared wide tree, or adds
    /// `extra_blob` at the root when given (to build an object that can be deleted later).
    fn commit(&mut self, parents: &[ObjectId], extra_blob: Option<ObjectId>) -> ObjectId {
        self.serial += 1;
        let n = self.serial;
        let (d, f) = (n % DIRS, (n / DIRS) % FILES_PER_DIR);
        self.files[d][f] = self
            .repo
            .odb()
            .write(ObjectKind::Blob, format!("change {n}\n").as_bytes())
            .expect("write changed blob");
        let mut tree = self.write_tree(&self.files);
        if let Some(blob) = extra_blob {
            let mut root = grit_lib::objects::parse_tree(
                &self.repo.odb().read(&tree).expect("read root tree").data,
            )
            .expect("parse root tree");
            root.push(TreeEntry::new(b"zz-extra", FileMode::REGULAR, blob));
            tree = self
                .repo
                .odb()
                .write(ObjectKind::Tree, &serialize_tree(&root))
                .expect("write extended tree");
        }
        let mut body = format!("tree {tree}\n");
        for parent in parents {
            body.push_str(&format!("parent {parent}\n"));
        }
        let when = 1_400_000_000 + n;
        body.push_str(&format!(
            "author A U Thor <author@example.com> {when} +0000\n\
             committer C O Mitter <committer@example.com> {when} +0000\n\ncommit {n}\n"
        ));
        self.repo
            .odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("write commit")
    }

    fn annotated_tag(&self, target: ObjectId, name: &str) -> ObjectId {
        let body = format!(
            "object {target}\ntype commit\ntag {name}\n\
             tagger T Agger <tagger@example.com> 1400000000 +0000\n\nrelease\n"
        );
        self.repo
            .odb()
            .write(ObjectKind::Tag, body.as_bytes())
            .expect("write tag")
    }
}

/// Commits of interest in the generated history.
struct History {
    early: ObjectId,
    tagged: ObjectId,
    tag: ObjectId,
    feature_tip: ObjectId,
    tip: ObjectId,
    topic_side: ObjectId,
    lost: ObjectId,
}

fn build_history(repo: &Repository) -> History {
    let mut b = HistoryBuilder::new(repo);
    let mut head = b.commit(&[], None);
    let early = head;
    for _ in 1..LINEAR_COMMITS {
        head = b.commit(&[head], None);
    }
    let tagged = head;
    let tag = b.annotated_tag(tagged, "v1");
    for i in 0..DIAMONDS {
        let left = b.commit(&[head], None);
        let right = b.commit(&[head], None);
        head = b.commit(&[left, right], None);
        if i == DIAMONDS / 2 {
            grit_test_support::refs::set_ref(&repo, "refs/heads/feature", &head)
                .expect("feature ref");
        }
    }
    let feature_tip = repo
        .ref_store()
        .resolve("refs/heads/feature")
        .expect("resolve feature");
    let tip = b.commit(&[head], None);
    let topic_side = b.commit(&[feature_tip], None);
    let lost = b.commit(&[tip], None);

    grit_test_support::refs::set_ref(&repo, "refs/heads/main", &tip).expect("main ref");
    grit_test_support::refs::set_ref(&repo, "refs/heads/topic", &topic_side).expect("topic ref");
    grit_test_support::refs::set_ref(&repo, "refs/tags/v1", &tag).expect("tag ref");
    History {
        early,
        tagged,
        tag,
        feature_tip,
        tip,
        topic_side,
        lost,
    }
}

fn entry(old: ObjectId, new: ObjectId, when: i64, message: &str) -> ReflogEntry {
    ReflogEntry {
        old_oid: old,
        new_oid: new,
        identity: reflog_identity(when),
        message: message.to_owned(),
    }
}

fn write_reflog(git_dir: &Path, refname: &str, entries: &[ReflogEntry]) {
    let body = entries
        .iter()
        .map(|e| {
            format!(
                "{} {} {}\t{}\n",
                e.old_oid, e.new_oid, e.identity, e.message
            )
        })
        .collect::<String>();
    let path = reflog_file_path(git_dir, refname);
    fs::create_dir_all(path.parent().expect("reflog parent")).expect("mkdir logs");
    fs::write(path, body).expect("write reflog");
}

/// Compare the logs of `refnames` as text, for readable failures.
fn assert_logs_text_match(git_side: &Path, grit_side: &Path, refnames: &[&str]) {
    for refname in refnames {
        let read = |worktree: &Path| {
            fs::read_to_string(reflog_file_path(&worktree.join(".git"), refname))
                .expect("read reflog")
        };
        assert_eq!(read(grit_side), read(git_side), "reflog of {refname}");
    }
}

/// Compare the logs of `refnames` as text, then every log file byte for byte.
fn assert_logs_match(git_side: &Path, grit_side: &Path, refnames: &[&str]) {
    assert_logs_text_match(git_side, grit_side, refnames);
    assert_reflog_tree_matches(git_side.join(".git"), &grit_side.join(".git"));
}

/// A files-backend repository with `main` checked out and the generated history.
fn seed_repo() -> (tempfile::TempDir, History) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        dir.path(),
        &InitOptions::new().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let history = build_history(&repo);
    (dir, history)
}

/// Expire `refnames` with grit on a separate thread, failing the test if it takes longer
/// than [`TIME_LIMIT`]; returns the prune count per ref.
fn grit_expire_within_limit(
    worktree: PathBuf,
    refnames: &'static [&'static str],
    params: ReflogExpireParams,
    explicit_total: Option<i64>,
    explicit_unreachable: Option<i64>,
) -> Vec<usize> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let repo = open_repo(&worktree);
        let cfg = ConfigSet::load(
            &Environment::empty(),
            Some(repo.git_dir()),
            &crate::config::LoadConfigOptions::standard(
                &Environment::empty(),
                Some(repo.git_dir()),
            ),
        )
        .expect("load config");
        let gc = load_gc_reflog_expire_config(&cfg, NOW);
        let pruned = refnames
            .iter()
            .map(|refname| {
                let result = expire_reflog_git(
                    &repo,
                    refname,
                    &params,
                    explicit_total,
                    explicit_unreachable,
                    &gc.patterns,
                    gc.global_total,
                    gc.global_unreachable,
                    NOW,
                )
                .expect("expire_reflog_git");
                assert!(result.actions.iter().all(|a| a.refname == *refname));
                result
                    .actions
                    .iter()
                    .filter(|a| a.action == ReflogExpireActionKind::Prune)
                    .count()
            })
            .collect::<Vec<_>>();
        let _ = tx.send(pruned);
    });
    rx.recv_timeout(TIME_LIMIT)
        .unwrap_or_else(|_| panic!("grit reflog expiry did not finish within {TIME_LIMIT:?}"))
}

#[test]
fn expire_unreachable_with_deep_merged_history_matches_git_quickly() {
    expire_unreachable_case(None);
}

/// A total cutoff in the middle of history: the reachability walk first stops there and has
/// to dig further for the entries naming early commits.
#[test]
fn expire_unreachable_with_total_cutoff_mid_history_matches_git() {
    expire_unreachable_case(Some(MID_HISTORY));
}

/// Expire every test ref with `--expire=<total>` (never when `None`) and the unreachable
/// cutoff, with grit and with git, and compare.
fn expire_unreachable_case(total: Option<i64>) {
    let (seed, h) = seed_repo();
    let git_dir = seed.path().join(".git");
    let z = ObjectId::zero();
    write_reflog(
        &git_dir,
        "refs/heads/main",
        &[
            entry(z, h.early, OLD, "reachable old"),
            entry(h.early, h.lost, OLD, "to lost commit"),
            entry(h.lost, h.tip, RECENT, "recent"),
        ],
    );
    write_reflog(
        &git_dir,
        "refs/heads/feature",
        &[
            entry(z, h.topic_side, OLD, "commit only on topic"),
            entry(h.topic_side, h.feature_tip, OLD, "back to feature"),
            entry(z, h.feature_tip, OLD, "feature tip"),
        ],
    );
    write_reflog(
        &git_dir,
        "refs/tags/v1",
        &[
            entry(z, h.tag, OLD, "annotated tag"),
            entry(z, h.tagged, OLD, "tagged commit"),
            entry(z, h.lost, OLD, "lost commit"),
        ],
    );
    write_reflog(
        &git_dir,
        "HEAD",
        &[
            entry(z, h.topic_side, OLD, "reachable from topic"),
            entry(h.topic_side, h.lost, OLD, "to lost commit"),
            entry(h.lost, h.tip, RECENT, "recent"),
        ],
    );
    const REFS: &[&str] = &[
        "refs/heads/main",
        "refs/heads/feature",
        "refs/tags/v1",
        "HEAD",
    ];

    let grit_copy = tempfile::tempdir().expect("grit copy");
    let git_copy = tempfile::tempdir().expect("git copy");
    copy_worktree(seed.path(), grit_copy.path());
    copy_worktree(seed.path(), git_copy.path());

    let pruned = grit_expire_within_limit(
        grit_copy.path().to_path_buf(),
        REFS,
        ReflogExpireParams {
            stale_fix: false,
            dry_run: false,
            verbose: false,
        },
        Some(total.unwrap_or(0)),
        Some(UNREACHABLE_CUTOFF),
    );
    // Git 2.35 through 2.46.0 did not peel an annotated-tag tip for this check and expired
    // every old entry of such a ref; only compare that ref with a Git that has the fix.
    let git_peels_tag_tips = git_version_at_least(2, 47);
    let compared = REFS
        .iter()
        .copied()
        .filter(|r| git_peels_tag_tips || *r != "refs/tags/v1")
        .collect::<Vec<_>>();
    let expire = total.map_or_else(|| "--expire=never".to_owned(), |t| format!("--expire={t}"));
    let cutoff = format!("--expire-unreachable={UNREACHABLE_CUTOFF}");
    for refname in &compared {
        git(
            git_copy.path(),
            &["reflog", "expire", &expire, &cutoff, refname],
        );
    }

    if git_peels_tag_tips {
        assert_logs_match(git_copy.path(), grit_copy.path(), &compared);
    } else {
        assert_logs_text_match(git_copy.path(), grit_copy.path(), &compared);
    }
    let tag_log = fs::read_to_string(reflog_file_path(
        &grit_copy.path().join(".git"),
        "refs/tags/v1",
    ))
    .expect("read tag reflog");
    let kept = tag_log
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .collect::<Vec<_>>();
    assert_eq!(kept, ["annotated tag", "tagged commit"]);
    assert_eq!(pruned, vec![1, 2, 1, 1], "per-ref prune counts");
    assert!(git_fsck_strict(grit_copy.path()));
}

/// Many branches expired in one pass share what the pass has read, but each judges
/// reachability from its own tip: an old entry naming a commit only another branch reaches
/// is pruned, whether that branch was expired before or after it.
#[test]
fn expire_many_refs_in_one_pass_judges_each_ref_by_its_own_tip_like_git() {
    const BRANCHES: usize = 40;
    let (seed, h) = seed_repo();
    let git_dir = seed.path().join(".git");
    let repo = open_repo(seed.path());
    let mut b = HistoryBuilder::new(&repo);
    let sides = (0..BRANCHES)
        .map(|_| b.commit(&[h.tagged], None))
        .collect::<Vec<_>>();
    drop(repo);
    let z = ObjectId::zero();
    let mut refnames = Vec::new();
    for (i, side) in sides.iter().enumerate() {
        let refname = format!("refs/heads/b{i}");
        grit_test_support::refs::set_ref(&grit_test_support::refs::at(&git_dir), &refname, side)
            .expect("branch ref");
        let other = sides[(i + 1) % BRANCHES];
        write_reflog(
            &git_dir,
            &refname,
            &[
                entry(z, h.early, OLD, "reachable old"),
                entry(h.early, other, OLD, "only on the next branch"),
                entry(other, *side, RECENT, "recent"),
            ],
        );
        refnames.push(refname);
    }
    write_reflog(
        &git_dir,
        "HEAD",
        &[
            entry(z, sides[7], OLD, "reachable from a branch"),
            entry(sides[7], h.lost, OLD, "to lost commit"),
            entry(h.lost, h.tip, RECENT, "recent"),
        ],
    );
    refnames.push("HEAD".to_owned());

    let grit_copy = tempfile::tempdir().expect("grit copy");
    let git_copy = tempfile::tempdir().expect("git copy");
    copy_worktree(seed.path(), grit_copy.path());
    copy_worktree(seed.path(), git_copy.path());

    let (tx, rx) = mpsc::channel();
    let worktree = grit_copy.path().to_path_buf();
    std::thread::spawn(move || {
        let outcome = open_repo(&worktree)
            .reflogs()
            .expire(
                &ExpireOptions::new(NOW)
                    .with_expire(Some("never".into()))
                    .with_expire_unreachable(Some(UNREACHABLE_CUTOFF.to_string())),
            )
            .expect("expire");
        let _ = tx.send(outcome);
    });
    let outcome = rx
        .recv_timeout(TIME_LIMIT)
        .unwrap_or_else(|_| panic!("grit reflog expiry did not finish within {TIME_LIMIT:?}"));
    git(
        git_copy.path(),
        &[
            "reflog",
            "expire",
            "--expire=never",
            &format!("--expire-unreachable={UNREACHABLE_CUTOFF}"),
            "--all",
        ],
    );

    let compared = refnames.iter().map(String::as_str).collect::<Vec<_>>();
    assert_logs_match(git_copy.path(), grit_copy.path(), &compared);
    assert_eq!(outcome.entries_pruned, BRANCHES + 1);
    for refname in &refnames {
        let result = outcome.refs.iter().find(|r| &r.ref_name == refname);
        assert_eq!(result.map(|r| r.pruned), Some(1), "pruned from {refname}");
    }
    assert!(git_fsck_strict(grit_copy.path()));
}

#[test]
fn stale_fix_prunes_entries_with_missing_objects_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        dir.path(),
        &InitOptions::new().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let h = build_history(&repo);
    let doomed = repo
        .odb()
        .write(ObjectKind::Blob, b"only in the broken commit\n")
        .expect("write doomed blob");
    let broken = HistoryBuilder::new(&repo).commit(&[h.tip], Some(doomed));
    let hex = doomed.to_hex();
    fs::remove_file(
        repo.git_dir()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]),
    )
    .expect("delete loose blob");
    drop(repo);

    let z = ObjectId::zero();
    write_reflog(
        &dir.path().join(".git"),
        "refs/heads/main",
        &[
            entry(z, h.early, OLD, "complete history"),
            entry(h.early, broken, OLD, "tree misses a blob"),
            entry(broken, h.tip, RECENT, "back to the tip"),
            entry(h.tip, h.tip, RECENT, "complete again"),
        ],
    );
    const REFS: &[&str] = &["refs/heads/main"];

    let grit_copy = tempfile::tempdir().expect("grit copy");
    let git_copy = tempfile::tempdir().expect("git copy");
    copy_worktree(dir.path(), grit_copy.path());
    copy_worktree(dir.path(), git_copy.path());

    let pruned = grit_expire_within_limit(
        grit_copy.path().to_path_buf(),
        REFS,
        ReflogExpireParams {
            stale_fix: true,
            dry_run: false,
            verbose: false,
        },
        Some(0),
        Some(0),
    );
    git(
        git_copy.path(),
        &[
            "reflog",
            "expire",
            "--stale-fix",
            "--expire=never",
            "--expire-unreachable=never",
            "refs/heads/main",
        ],
    );

    assert_logs_match(git_copy.path(), grit_copy.path(), REFS);
    assert_eq!(pruned, vec![2]);
}

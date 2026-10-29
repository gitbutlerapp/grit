//! Status must not rewrite `.git/index` when optional locks are disabled (issue #2290).

mod support;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::porcelain::status::{status, InterruptedRewrite, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn index_digest(path: &Path) -> String {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("sha256sum");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("digest")
        .to_owned()
}

fn touch_tracked(repo_root: &Path) {
    let listing = git(repo_root, &["ls-files", "-z"]);
    for path in listing.split('\0').filter(|p| !p.is_empty()) {
        filetime::set_file_mtime(repo_root.join(path), filetime::FileTime::now()).expect("touch");
    }
}

fn repo_with_stale_index_stat(file_count: usize) -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    for i in 0..file_count {
        fs::write(repo_root.join(format!("f{i}.txt")), format!("{i}\n")).expect("write");
    }
    git(repo_root, &["add", "."]);
    git(repo_root, &["commit", "-qm", "init"]);
    std::thread::sleep(Duration::from_millis(1200));
    touch_tracked(repo_root);
    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");
    (tmp, grit_repo)
}

#[test]
fn status_write_index_false_leaves_index_bytes_and_mtime() {
    let (_tmp, grit_repo) = repo_with_stale_index_stat(50);
    let index_path = grit_repo.index_handle().path();
    let digest_before = index_digest(&index_path);
    let mtime_before = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("mtime");

    let model = status(
        &grit_repo,
        &StatusOptions {
            write_index: false,
            ..StatusOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(model.unstaged.is_empty());

    assert_eq!(index_digest(&index_path), digest_before);
    assert_eq!(
        fs::metadata(&index_path)
            .expect("index metadata")
            .modified()
            .expect("mtime"),
        mtime_before
    );
    assert!(
        !grit_repo.git_dir().join("index.lock").exists(),
        "read-only status must not create index.lock"
    );
}

#[test]
fn status_honors_git_optional_locks_zero_in_environment() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);
    for i in 0..50 {
        fs::write(repo_root.join(format!("f{i}.txt")), format!("{i}\n")).expect("write");
    }
    git(repo_root, &["add", "."]);
    git(repo_root, &["commit", "-qm", "init"]);
    std::thread::sleep(Duration::from_millis(1200));
    touch_tracked(repo_root);

    let env = Environment::from_vars(
        [(OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0"))],
        repo_root.to_path_buf(),
    );
    let grit_repo =
        Repository::discover(Some(repo_root), &RepositoryOptions::with_environment(env))
            .expect("discover");
    let index_path = grit_repo.index_handle().path();
    let digest_before = index_digest(&index_path);

    let _model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");

    assert_eq!(index_digest(&index_path), digest_before);
}

#[test]
fn grit_cli_no_optional_locks_leaves_index_unchanged() {
    let (_tmp, grit_repo) = repo_with_stale_index_stat(20);
    let repo_root = grit_repo.work_tree().expect("work tree");
    let index_path = grit_repo.index_handle().path();
    let digest_before = index_digest(&index_path);

    let grit_bin = std::env::var("CARGO_BIN_EXE_grit")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/grit")
        });
    let out = Command::new(&grit_bin)
        .arg("--no-optional-locks")
        .arg("status")
        .current_dir(repo_root)
        .output()
        .expect("grit status");
    assert!(
        out.status.success(),
        "grit status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(index_digest(&index_path), digest_before);
}

#[test]
fn status_read_only_reports_pending_rewrite_without_touching_journal() {
    use grit_lib::index::write_tree::write_tree_from_index;
    use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
    use grit_lib::objects::{parse_commit, parse_tree, ObjectKind};
    use grit_lib::revwalk::history::{
        apply_rewrite, plan_rewrite, rewrite_apply_journal_active, ApplyOptions, CommitEdit,
        EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
    };

    use support::{build_commit_dag, git_rev_parse, HistoryRepo};

    fn open_grit(wt: &Path) -> Repository {
        Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover")
    }

    fn apply_opts() -> ApplyOptions {
        ApplyOptions {
            identity: support::reflog_signature("Applier <apply@grit.test>"),
            reflog_message: "history rewrite".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        }
    }

    fn tree_plus_file(
        repo: &Repository,
        base_tree: grit_lib::objects::ObjectId,
        path: &str,
        content: &[u8],
    ) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
        let blob = repo.odb().write(ObjectKind::Blob, content)?;
        let raw = repo.odb().read(&base_tree)?;
        let mut index = Index::new();
        index.hash_algo = repo.odb().hash_algo();
        for entry in parse_tree(&raw.data)? {
            let name = String::from_utf8_lossy(&entry.name).into_owned();
            index.add_or_replace(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: entry.mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid: entry.oid,
                flags: name.len().min(0xfff) as u16,
                flags_extended: None,
                path: name.as_bytes().to_vec(),
                base_index_pos: 0,
            });
        }
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: content.len() as u32,
            oid: blob,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        });
        write_tree_from_index(repo.odb(), &index, "")
    }

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let target = oids["b"];
    let parent_tree = parse_commit(&grit.odb().read(&target).expect("read").data)
        .expect("parse")
        .tree;
    let new_tree =
        tree_plus_file(&grit, parent_tree, "rewritten-only.txt", b"from rewrite\n").expect("tree");
    let mut edits = HistoryEdits::new();
    edits.insert(
        target,
        CommitEdit::Rewrite {
            tree: Some(new_tree),
            message: Some("tree change\n".into()),
            author: None,
            parents: None,
        },
    );
    let plan = plan_rewrite(
        &grit,
        &edits,
        &RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: "Applier <apply@grit.test> 1 +0000"
                .parse()
                .expect("signature"),
            preserve_committer: false,
            sign: true,
        },
    )
    .expect("plan");
    let tip_before = git_rev_parse(wt, "refs/heads/main").expect("tip");
    grit_lib::repo::set_test_inject_rewrite_apply_abort_after_refs(true);
    let _err = apply_rewrite(&grit, &plan, &apply_opts(), &mut NullProgress, &NeverCancel)
        .expect_err("injected abort");
    grit_lib::repo::set_test_inject_rewrite_apply_abort_after_refs(false);
    assert!(rewrite_apply_journal_active(grit.git_dir()));
    assert_ne!(git_rev_parse(wt, "refs/heads/main"), Some(tip_before));

    let model = status(
        &grit,
        &StatusOptions {
            write_index: false,
            ..StatusOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert_eq!(model.interrupted_rewrite, Some(InterruptedRewrite::Pending));
    assert!(rewrite_apply_journal_active(grit.git_dir()));
}

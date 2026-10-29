//! Issue #2312: conflicted wide merges must not hold multiple full indexes at once.
//!
//! Runs the merge checkout in a fresh process so peak RSS reflects the operation, not the
//! whole integration binary. The fixture matches the dogfood shape at reduced scale.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{abort_branch_merge, integrate_branch, IntegrateBranchOutcome};
use grit_lib::repo::Repository;
use grit_test_support::git;

const DIRS: usize = 200;
const FILES_PER_DIR: usize = 300;
const CONFLICT_DIRS: usize = 10;
const WA_DIRS: usize = 100;
const WB_NONCONFLICT_START: usize = 100;
const WB_NONCONFLICT_DIRS: usize = 100;

// Debug child on 60k paths (200×300 files, 3k conflicts): peak ~128 MiB after fix; budget
// rejects the old multi-index path that exceeded ~180 MiB in the same fixture shape.
const MAX_MERGE_RSS_KIB: i64 = 148 * 1024;
const MAX_ABORT_RSS_KIB: i64 = 158 * 1024;

fn peak_rss_kib() -> i64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    i64::from(unsafe { usage.assume_init() }.ru_maxrss)
}

fn file_path(root: &Path, dir: usize, file: usize) -> std::path::PathBuf {
    root.join(format!(
        "src/mod{dir:03}/pkg{dir:03}/file{file:04}.txt",
        dir = dir,
        file = file
    ))
}

fn write_base(root: &Path) {
    for dir in 0..DIRS {
        for file in 0..FILES_PER_DIR {
            let path = file_path(root, dir, file);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(path, format!("line1\nline2 base {dir}/{file}\n")).expect("write");
        }
    }
}

fn edit_line2(root: &Path, dir: usize, text: &str) {
    for file in 0..FILES_PER_DIR {
        let path = file_path(root, dir, file);
        let body = fs::read_to_string(&path).expect("read");
        let mut lines: Vec<_> = body.lines().collect();
        lines[1] = text;
        fs::write(path, format!("{}\n", lines.join("\n"))).expect("write");
    }
}

fn oid(root: &Path, rev: &str) -> ObjectId {
    git(root, &["rev-parse", rev]).trim().parse().expect("oid")
}

#[test]
fn merge_conflict_child() {
    let Some(root) = std::env::var_os("GRIT_MERGE_CONFLICT_RSS_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let mode = std::env::var("GRIT_MERGE_CONFLICT_RSS_MODE").unwrap_or_default();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let wa = oid(root, "wa");
    let wb = oid(root, "wb");

    if mode == "abort" {
        integrate_branch(
            &repo,
            "refs/heads/wa",
            wa,
            wb,
            "wb",
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("merge");
        abort_branch_merge(&repo).expect("abort");
        let rss = peak_rss_kib();
        println!("mode=abort peak_rss_kib={rss}");
        assert!(rss < MAX_ABORT_RSS_KIB, "abort peaked at {rss} KiB");
        return;
    }

    let outcome = integrate_branch(
        &repo,
        "refs/heads/wa",
        wa,
        wb,
        "wb",
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("integrate");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    let rss = peak_rss_kib();
    println!("mode=merge peak_rss_kib={rss}");
    assert!(rss < MAX_MERGE_RSS_KIB, "merge peaked at {rss} KiB");
}

#[test]
fn wide_conflict_merge_memory_and_git_compatibility() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Wide Merge"]);
    git(root, &["config", "user.email", "wide@example.com"]);
    write_base(root);
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["branch", "-q", "wa"]);
    git(root, &["branch", "-q", "wb"]);

    git(root, &["checkout", "-q", "wa"]);
    for dir in 0..WA_DIRS {
        edit_line2(root, dir, &format!("line2 A changed {dir}"));
    }
    git(root, &["commit", "-qam", "wa"]);

    git(root, &["checkout", "-q", "wb"]);
    for dir in WB_NONCONFLICT_START..(WB_NONCONFLICT_START + WB_NONCONFLICT_DIRS) {
        edit_line2(root, dir, &format!("line2 B changed {dir}"));
    }
    for dir in 0..CONFLICT_DIRS {
        edit_line2(root, dir, &format!("line2 Bconf changed {dir}"));
    }
    git(root, &["commit", "-qam", "wb"]);

    git(root, &["checkout", "-q", "wa"]);
    let wa_before = git(root, &["rev-parse", "HEAD"]);
    assert!(
        !grit_test_support::git_cmd(&["merge", "--no-edit", "wb"])
            .in_dir(root)
            .exec()
            .ok(),
        "git merge should stop on conflicts"
    );
    assert!(
        !git(root, &["status", "--porcelain"]).trim().is_empty(),
        "git should stop with conflicts"
    );
    git(root, &["merge", "--abort"]);
    assert_eq!(git(root, &["rev-parse", "HEAD"]), wa_before);

    for mode in ["merge", "abort"] {
        git(root, &["checkout", "-q", "-f", "wa"]);
        git(root, &["reset", "--hard", "wa"]);
        let mut child = Command::new(std::env::current_exe().expect("test binary"));
        child
            .args(["--exact", "merge_conflict_child", "--nocapture"])
            .env("GRIT_MERGE_CONFLICT_RSS_ROOT", root)
            .env("GRIT_MERGE_CONFLICT_RSS_MODE", mode);
        let out = child.output().expect("child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "mode={mode}: {stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        eprintln!("{stdout}");
    }

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    git(root, &["checkout", "-q", "wa"]);
    let outcome = integrate_branch(
        &repo,
        "refs/heads/wa",
        oid(root, "wa"),
        oid(root, "wb"),
        "wb",
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit merge");
    let IntegrateBranchOutcome::Conflicts { paths, .. } = outcome else {
        panic!("expected conflicts");
    };
    assert_eq!(paths.len(), CONFLICT_DIRS * FILES_PER_DIR);
    abort_branch_merge(&repo).expect("grit abort");
    assert!(
        git(root, &["status", "--porcelain"]).trim().is_empty(),
        "abort should leave a clean tree"
    );
}

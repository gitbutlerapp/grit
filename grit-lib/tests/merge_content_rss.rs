//! Issue #2036: wide clean content merges must not retain duplicate path/entry maps.
//! Each measured operation runs after exec so fixture construction and other tests cannot
//! hide growth behind an earlier process high-water mark.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::diff::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::repo::Repository;
use grit_test_support::git;

const FILES: usize = 20_000;
// Fresh-process debug measurements: loose tree/full merges use about 56/59 MiB;
// packs additionally retain mapped pages and bounded ODB/delta caches. Leave allocator
// headroom for each layout while rejecting the old ~78/95 MiB loose merge peaks.
const MAX_LOOSE_RSS_KIB: i64 = 65 * 1024;
const MAX_PACKED_RSS_KIB: i64 = 88 * 1024;

fn oid(root: &Path, rev: &str) -> ObjectId {
    git(root, &["rev-parse", rev]).trim().parse().expect("oid")
}

fn peak_rss_kib() -> i64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: usage is writable storage for rusage, initialised on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    // SAFETY: getrusage succeeded.
    i64::from(unsafe { usage.assume_init() }.ru_maxrss)
}

#[test]
fn content_merge_child() {
    let Some(root) = std::env::var_os("GRIT_CONTENT_MERGE_RSS_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let into = oid(root, "main");
    let other = oid(root, "feat");
    let tree = if std::env::var_os("GRIT_CONTENT_MERGE_RSS_TREE_ONLY").is_some() {
        let output = merge_trees_three_way(
            &repo,
            oid(root, "main^1^{tree}"),
            oid(root, "main^{tree}"),
            oid(root, "feat^{tree}"),
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
        )
        .expect("merge trees");
        assert_eq!(output.index.entries().len(), FILES);
        assert!(output
            .index
            .entries()
            .iter()
            .all(|entry| entry.stage() == 0));
        assert!(output.conflict_content.is_empty());
        output.result_tree
    } else {
        assert!(matches!(
            integrate_branch(&repo, "refs/heads/main", into, other, "feat").expect("merge"),
            IntegrateBranchOutcome::Merged { .. }
        ));
        oid(root, "HEAD^{tree}")
    };
    let max_rss = if std::env::var_os("GRIT_CONTENT_MERGE_RSS_PACKED").is_some() {
        MAX_PACKED_RSS_KIB
    } else {
        MAX_LOOSE_RSS_KIB
    };
    let rss = peak_rss_kib();
    println!("tree={tree} peak_rss_kib={rss}");
    assert!(
        rss < max_rss,
        "20k content merge peaked at {rss} KiB; budget {max_rss} KiB"
    );
}

fn body(i: usize) -> String {
    (0..20).map(|j| format!("line {j} of {i}\n")).collect()
}

#[test]
fn wide_content_merge_memory_and_git_compatibility() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Mono Dev"]);
    git(root, &["config", "user.email", "mono@example.com"]);
    git(root, &["config", "gc.auto", "0"]);
    let paths: Vec<_> = (0..FILES)
        .map(|i| root.join(format!("d{:03}/f{i}.txt", i % 200)))
        .collect();
    for (i, path) in paths.iter().enumerate() {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, body(i)).expect("base");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-qc", "feat"]);
    for (i, path) in paths.iter().enumerate() {
        fs::write(path, format!("{}feat tail\n", body(i))).expect("feat");
    }
    git(root, &["commit", "-qam", "feat"]);
    git(root, &["switch", "-q", "main"]);
    for (i, path) in paths.iter().enumerate() {
        fs::write(path, format!("main head\n{}", body(i))).expect("main");
    }
    git(root, &["commit", "-qam", "main"]);
    let main = oid(root, "main").to_hex();
    let expected = git(root, &["merge-tree", "--write-tree", "main", "feat"]);
    for packed in [false, true] {
        if packed {
            git(root, &["gc", "--quiet"]);
        }
        for tree_only in [true, false] {
            let mut child = Command::new(std::env::current_exe().expect("test binary"));
            child
                .args([
                    "--exact",
                    "merge_content_rss::content_merge_child",
                    "--nocapture",
                ])
                .env("GRIT_CONTENT_MERGE_RSS_ROOT", root);
            child.env_remove("GRIT_CONTENT_MERGE_RSS_TREE_ONLY");
            child.env_remove("GRIT_CONTENT_MERGE_RSS_PACKED");
            if tree_only {
                child.env("GRIT_CONTENT_MERGE_RSS_TREE_ONLY", "1");
            }
            if packed {
                child.env("GRIT_CONTENT_MERGE_RSS_PACKED", "1");
            }
            let out = child.output().expect("child");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "packed={packed} tree_only={tree_only}: {stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                stdout.contains(&format!("tree={}", expected.trim())),
                "{stdout}"
            );
            eprintln!("packed={packed} tree_only={tree_only}: {stdout}");
        }
        assert_eq!(oid(root, "HEAD^{tree}").to_hex(), expected.trim());
        assert_eq!(git(root, &["write-tree"]).trim(), expected.trim());
        assert!(git(root, &["status", "--porcelain"]).trim().is_empty());
        for (i, path) in paths.iter().enumerate() {
            assert_eq!(
                fs::read_to_string(path).expect("merged file"),
                format!("main head\n{}feat tail\n", body(i))
            );
        }
        git(root, &["fsck", "--strict"]);
        git(root, &["reset", "--hard", &main]);
    }
}

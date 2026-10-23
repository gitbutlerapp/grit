//! Dry-run bundle plans match written bundles and leave repositories unchanged.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use grit_lib::bundle::bundle_remote::{resolve_bundle_spec, BundleCreateOptions};
use grit_lib::bundle::Bundle;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_two_commit_repo(dir: &Path) {
    git_ok(dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git_ok(dir, &["add", "a.txt"]);
    git_ok(dir, &["commit", "-q", "-m", "first"]);
    std::fs::write(dir.join("b.txt"), b"two\n").unwrap();
    git_ok(dir, &["add", "b.txt"]);
    git_ok(dir, &["commit", "-q", "-m", "second"]);
}

fn ref_map(header: &grit_lib::bundle::BundleHeader) -> HashMap<String, String> {
    header
        .refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.to_hex()))
        .collect()
}

fn git_dir_fingerprint(git_dir: &Path) -> String {
    let mut parts = Vec::new();
    for entry in walkdir(git_dir) {
        parts.push(entry);
    }
    parts.sort();
    parts.join("\n")
}

fn walkdir(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = entry.metadata() {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                out.push(format!(
                    "{}:{}:{}",
                    rel.display(),
                    meta.len(),
                    meta.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                ));
            }
        }
    }
    out
}

#[test]
fn bundle_plan_create_matches_written_bundle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path();
    init_two_commit_repo(src);
    let repo = Repository::discover(Some(src), &RepositoryOptions::empty()).expect("open");
    let options = BundleCreateOptions {
        all_refs: false,
        rev_tokens: vec!["HEAD~1..HEAD".to_owned()],
    };
    let plan = repo
        .bundles()
        .plan_create_with_options(&options)
        .expect("plan");
    let spec = resolve_bundle_spec(&repo, &options).expect("spec");
    let bundle_path = src.join("out.bundle");
    repo.bundles()
        .create(&spec, &bundle_path)
        .expect("write bundle");

    git_ok(src, &["bundle", "verify", bundle_path.to_str().unwrap()]);

    let written = Bundle::open(&bundle_path).expect("open written");
    let header = written.header();
    assert_eq!(plan.refs.len(), header.refs.len());
    assert_eq!(
        plan.refs
            .iter()
            .map(|r| (r.name.as_str(), r.oid.to_hex()))
            .collect::<Vec<_>>(),
        header
            .refs
            .iter()
            .map(|(name, oid)| (name.as_str(), oid.to_hex()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        plan.prerequisites
            .iter()
            .map(|p| (p.oid.to_hex(), p.subject.clone()))
            .collect::<Vec<_>>(),
        header
            .prerequisites
            .iter()
            .map(|(oid, subject)| (oid.to_hex(), subject.clone()))
            .collect::<Vec<_>>()
    );
    let written_count = written.pack_object_count().expect("pack count");
    assert_eq!(plan.object_count, written_count);
    let _ = ref_map(header);
}

#[test]
fn bundle_plan_unbundle_writes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path();
    init_two_commit_repo(src);
    let bundle_path = src.join("transfer.bundle");
    git_ok(
        src,
        &["bundle", "create", bundle_path.to_str().unwrap(), "HEAD"],
    );

    let dest = tempfile::tempdir().expect("dest");
    git_ok(dest.path(), &["init", "-q", "-b", "main", "."]);
    let dest_repo =
        Repository::discover(Some(dest.path()), &RepositoryOptions::empty()).expect("dest");
    let before = git_dir_fingerprint(dest_repo.git_dir());

    let plan = dest_repo
        .bundles()
        .plan_unbundle(&bundle_path)
        .expect("plan unbundle");
    assert!(plan.verify.is_ok() || !plan.verify.missing_prerequisites.is_empty());

    let after = git_dir_fingerprint(dest_repo.git_dir());
    assert_eq!(before, after, "dry-run unbundle must not touch .git");

    git_ok(dest.path(), &["fsck", "--strict"]);
}

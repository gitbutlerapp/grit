//! Forced tag replacement and dry-run behavior vs system `git`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::tag::{create_tag, list_tags, CreateTagRequest, TagKind};
use grit_lib::repo::Repository;
use std::process::Command;

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn seed_repo(root: &std::path::Path) -> (Repository, grit_lib::objects::ObjectId) {
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "x\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "seed"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    (repo, head)
}

#[test]
fn force_replace_lightweight_with_annotated_git_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (repo, head) = seed_repo(&root);
    git(&root, &["tag", "v1"]);

    let lightweight = list_tags(&repo).unwrap()[0].oid;
    let tagger = "Tagger <t@example.com> 946684800 +0000";
    let outcome = create_tag(
        &repo,
        &CreateTagRequest {
            name: "v1".into(),
            target: head,
            message: Some("annotated release\n".into()),
            tagger: Some(tagger.into()),
            sign: None,
            signing_key: None,
            tree_sha256: None,
            force: true,
            dry_run: false,
        },
    )
    .expect("force annotated");

    assert_eq!(outcome.kind, TagKind::Annotated);
    assert_eq!(outcome.replaced, Some(lightweight));
    assert_ne!(outcome.oid, lightweight);

    git(&root, &["fsck"]);
    let cat_out = Command::new("git")
        .args(["cat-file", "-p", "v1"])
        .current_dir(&root)
        .output()
        .expect("cat-file");
    let cat = String::from_utf8_lossy(&cat_out.stdout);
    assert!(cat.contains("tag v1"));
    assert!(cat.contains("annotated release"));
}

#[test]
fn force_replace_annotated_with_lightweight_git_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (repo, head) = seed_repo(&root);
    git(&root, &["tag", "-a", "v1", "-m", "old"]);

    let annotated = list_tags(&repo).unwrap()[0].oid;
    let outcome = create_tag(
        &repo,
        &CreateTagRequest {
            name: "v1".into(),
            target: head,
            message: None,
            tagger: None,
            sign: None,
            signing_key: None,
            tree_sha256: None,
            force: true,
            dry_run: false,
        },
    )
    .expect("force lightweight");

    assert_eq!(outcome.kind, TagKind::Lightweight);
    assert_eq!(outcome.replaced, Some(annotated));
    assert_eq!(outcome.oid, head);

    git(&root, &["fsck"]);
    let kind = Command::new("git")
        .args(["cat-file", "-t", "v1"])
        .current_dir(&root)
        .output()
        .expect("cat-file -t");
    assert_eq!(String::from_utf8_lossy(&kind.stdout).trim(), "commit");
}

#[test]
fn create_dry_run_leaves_refs_untouched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (repo, head) = seed_repo(&root);

    create_tag(
        &repo,
        &CreateTagRequest {
            name: "preview".into(),
            target: head,
            message: Some("note\n".into()),
            tagger: Some("T <t@example.com> 1700000000 +0000".into()),
            sign: None,
            signing_key: None,
            tree_sha256: None,
            force: false,
            dry_run: true,
        },
    )
    .expect("dry-run create");

    assert!(list_tags(&repo).unwrap().is_empty());

    git(&root, &["tag", "existing"]);
    create_tag(
        &repo,
        &CreateTagRequest {
            name: "existing".into(),
            target: head,
            message: Some("other\n".into()),
            tagger: Some("T <t@example.com> 1700000000 +0000".into()),
            sign: None,
            signing_key: None,
            tree_sha256: None,
            force: true,
            dry_run: true,
        },
    )
    .expect("dry-run force");

    let listed = list_tags(&repo).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind, TagKind::Lightweight);

    let obj = repo.odb().read(&listed[0].oid).unwrap();
    assert_eq!(obj.kind, ObjectKind::Commit);
}

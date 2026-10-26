//! Revision labels use Git-compatible refs and tag objects on both ref backends.

#[path = "support/refs_harness.rs"]
mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::object_inspect::RevisionLabel;
use grit_lib::objects::ObjectKind;
use grit_lib::refs;
use grit_lib::repo::Repository;
use support::{each_backend, git, Backend};

#[test]
fn revision_labels_read_system_git_tags_and_packed_refs() {
    for backend in [Backend::Files, Backend::Reftable] {
        if backend == Backend::Reftable && !support::require_reftable_git() {
            continue;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let wt = dir.path();
        let ref_format = match backend {
            Backend::Files => "--ref-format=files",
            Backend::Reftable => "--ref-format=reftable",
        };
        git(wt, &["init", "-b", "main", ref_format]);
        git(wt, &["commit", "--allow-empty", "-m", "initial"]);
        git(wt, &["tag", "light"]);
        git(wt, &["tag", "-a", "release", "-m", "release metadata"]);
        git(wt, &["pack-refs", "--all"]);
        let repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open repo");
        let head = git(wt, &["rev-parse", "HEAD"]);

        for spec in ["HEAD", "main"] {
            let label = repo.revision_label(spec).expect("branch label");
            assert!(
                matches!(&label, Some(RevisionLabel::Branch { name }) if name == "main"),
                "{backend:?} {spec}: {label:?}; HEAD={:?}; branch={:?}",
                refs::read_symbolic_ref(repo.git_dir(), "HEAD"),
                refs::resolve_ref(repo.git_dir(), "refs/heads/main")
            );
        }
        assert!(matches!(repo.revision_label("light").expect("tag label"),
            Some(RevisionLabel::Tag { name, annotation: None }) if name == "light"));
        let Some(RevisionLabel::Tag {
            name,
            annotation: Some(tag),
        }) = repo.revision_label("release").expect("annotation")
        else {
            panic!("expected tag");
        };
        assert_eq!(name, "release");
        assert_eq!(tag.object.to_hex(), head.trim());
        assert_eq!(tag.object_type, "commit");
        assert_eq!(tag.message, "release metadata\n");
        assert!(tag.tagger.is_some());
        git(wt, &["fsck", "--strict"]);
    }
}

#[test]
fn revision_labels_on_library_tags_are_readable_by_system_git() {
    each_backend(|_backend, fixture| {
        let wt = fixture.worktree();
        let repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open repo");
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("tree");
        let commit = repo.odb().write(ObjectKind::Commit, format!(
            "tree {tree}\nauthor Writer <writer@example.com> 1 +0000\ncommitter Writer <writer@example.com> 1 +0000\n\ninitial\n"
        ).as_bytes()).expect("commit");
        refs::write_ref(repo.git_dir(), "refs/heads/main", &commit).expect("branch");
        let tag = repo.odb().write(ObjectKind::Tag, format!(
            "object {commit}\ntype commit\ntag release\ntagger Writer <writer@example.com> 1 +0000\n\nrelease metadata\n"
        ).as_bytes()).expect("tag");
        refs::write_ref(repo.git_dir(), "refs/tags/release", &tag).expect("tag ref");
        assert!(matches!(repo.revision_label("release").expect("label"),
            Some(RevisionLabel::Tag { annotation: Some(annotation), .. }) if annotation.object == commit));
        assert_eq!(git(wt, &["cat-file", "-t", "release"]).trim(), "tag");
        assert_eq!(
            git(wt, &["rev-parse", "release^{}"]).trim(),
            commit.to_hex()
        );
        assert_eq!(
            git(wt, &["cat-file", "tag", "release"]),
            String::from_utf8(repo.odb().read(&tag).expect("tag object").data).expect("utf8")
        );
        git(wt, &["fsck", "--strict"]);
    });
}

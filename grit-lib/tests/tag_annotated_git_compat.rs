//! Annotated tag creation round-trips with system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_tag, ObjectKind};
use grit_lib::porcelain::tag::{create_tag, tag_ref_metadata, CreateTagRequest, TagKind};
use grit_lib::repo::Repository;
use std::process::Command;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn grit_annotated_tag_round_trips_with_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "x\n").unwrap();
    git(&root, &["add", "f"]);
    git(&root, &["commit", "-qm", "seed"]);

    let repo =
        Repository::discover(Some(root.as_path()), &RepositoryOptions::empty()).expect("discover");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").unwrap();
    let tagger = "Tagger <t@example.com> 946684800 +0000";
    let outcome = create_tag(
        &repo,
        &CreateTagRequest {
            name: "v1".to_owned(),
            target: head,
            message: Some("release\n".to_owned()),
            tagger: Some(tagger.to_owned()),
            sign: None,
            signing_key: None,
            tree_sha256: None,
        },
    )
    .expect("create annotated tag");
    assert_eq!(outcome.kind, TagKind::Annotated);
    assert_eq!(outcome.peeled, Some(head));

    git(&root, &["fsck"]);
    let cat = git(&root, &["cat-file", "-p", "v1"]);
    assert!(cat.contains("object "));
    assert!(cat.contains("tag v1"));
    assert!(cat.contains("release"));

    let (kind, peeled) = tag_ref_metadata(&repo, &outcome.oid);
    assert_eq!(kind, TagKind::Annotated);
    assert_eq!(peeled, Some(head));

    let obj = repo.odb().read(&outcome.oid).unwrap();
    assert_eq!(obj.kind, ObjectKind::Tag);
    let parsed = parse_tag(&obj.data).unwrap();
    assert_eq!(parsed.tag, "v1");
    assert_eq!(parsed.object, head);
}

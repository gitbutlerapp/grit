//! Annotated tag list metadata matches `git for-each-ref` (subject, tagger, date).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::tag::list_tags;
use grit_lib::refs::listing::{RefNamespace, RefQuery, RefSort, SortKey};
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

fn open(dir: &std::path::Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn git_tag_field(dir: &std::path::Path, name: &str, field: &str) -> String {
    git(
        dir,
        &[
            "for-each-ref",
            &format!("refs/tags/{name}"),
            &format!("--format=%({field})"),
        ],
    )
    .trim()
    .to_owned()
}

fn name_query() -> RefQuery {
    RefQuery {
        namespaces: vec![RefNamespace::Tags],
        sort: RefSort {
            key: SortKey::Name,
            reverse: false,
        },
        ..RefQuery::default()
    }
}

#[test]
fn annotation_fields_match_git_for_each_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["commit", "--allow-empty", "-q", "-m", "seed"]);
    git(
        &root,
        &[
            "tag",
            "-a",
            "release",
            "-m",
            "First line\n\nBody line",
            "HEAD",
        ],
    );
    git(&root, &["tag", "-a", "inner", "-m", "inner tag", "HEAD"]);
    git(&root, &["tag", "-a", "outer", "-m", "outer tag", "inner"]);

    let tree = git(&root, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
    git(&root, &["tag", "-a", "tree-tag", "-m", "tree", &tree]);

    let repo = open(&root);
    let tags = list_tags(&repo, &name_query()).unwrap();
    for tag in &tags {
        if tag.kind != grit_lib::porcelain::tag::TagKind::Annotated {
            continue;
        }
        let ann = tag.annotation.as_ref().expect("annotation");
        assert_eq!(
            ann.subject,
            git_tag_field(&root, &tag.name, "contents:subject"),
            "{}",
            tag.name
        );
        assert_eq!(
            ann.tagger.name,
            git_tag_field(&root, &tag.name, "taggername"),
            "{}",
            tag.name
        );
        assert_eq!(
            ann.date,
            git_tag_field(&root, &tag.name, "taggerdate:unix")
                .parse::<i64>()
                .expect("unix"),
            "{}",
            tag.name
        );
        match tag.target_kind {
            ObjectKind::Commit | ObjectKind::Tag | ObjectKind::Tree => {}
            other => panic!("unexpected target kind {other:?} for {}", tag.name),
        }
    }

    let tree_tag = tags
        .iter()
        .find(|t| t.name == "tree-tag")
        .expect("tree-tag");
    assert_eq!(tree_tag.target_kind, ObjectKind::Tree);
    let outer = tags.iter().find(|t| t.name == "outer").expect("outer");
    assert_eq!(outer.target_kind, ObjectKind::Tag);
    let _inner_oid: ObjectId = git(&root, &["rev-parse", "inner^{}"])
        .trim()
        .parse()
        .unwrap();
}

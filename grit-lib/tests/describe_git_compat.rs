//! `RevWalk::describe` distance matches `git rev-list --count tag..rev`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::ident::Signature;
use grit_lib::objects::{serialize_tag, ObjectId, ObjectKind, TagData};
use grit_lib::refs::store::RefName;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::DescribeOptions;

mod support;
use support::{git, git_interop_available, Backend};

#[test]
fn describe_distance_matches_git_rev_list_count() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    git(wt, &["init", "-q"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "one"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "two"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "three"]);
    git(wt, &["tag", "-a", "v0", "-m", "t", "HEAD~2"]);
    git(wt, &["tag", "-a", "v1", "-m", "t", "HEAD~1"]);

    let repo = Repository::open(
        &wt.join(".git"),
        &RepositoryOptions::empty().with_work_tree(wt),
    )
    .expect("open");
    let described = repo
        .rev_walk()
        .describe(
            "HEAD",
            &DescribeOptions {
                tags: true,
                ..Default::default()
            },
        )
        .expect("describe")
        .expect("some tag");
    let tag_short = described
        .tag
        .as_str()
        .strip_prefix("refs/tags/")
        .unwrap_or(described.tag.as_str());
    let git_count = git(wt, &["rev-list", "--count", &format!("{tag_short}..HEAD")])
        .trim()
        .parse::<usize>()
        .expect("count");
    assert_eq!(described.distance, git_count);
}

#[test]
fn describe_clock_skew_fixture_matches_git_rev_list_count() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path();
    let repo = Repository::init(wt, &InitOptions::default()).expect("init");
    let odb = repo.odb();
    let base = write_commit_at(odb, &[], "base", 3129).expect("base");
    let tagged = write_commit_at(odb, &[base], "tagged", 2598).expect("tagged");
    let side = write_commit_at(odb, &[tagged], "side", 4411).expect("side");
    let merge = write_commit_at(odb, &[base, side], "merge", 5000).expect("merge");
    grit_lib::refs::write_ref(repo.git_dir(), "refs/heads/main", &merge).expect("ref");
    tag_annotated(&repo, "t20", tagged, "m");

    let described = repo
        .rev_walk()
        .describe("main", &DescribeOptions::default())
        .expect("describe")
        .expect("tag");
    let git_count = git(wt, &["rev-list", "--count", "refs/tags/t20..main"])
        .trim()
        .parse::<usize>()
        .expect("count");
    assert_eq!(described.distance, git_count);
    assert_eq!(described.distance, 2);
}

/// Issue #2479: tag peel is merge first parent and side-branch root (two path depths).
#[test]
fn describe_diamond_merge_matches_git() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let wt = root.path();
    git(wt, &["init", "-q", "-b", "main"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "base"]);
    git(wt, &["tag", "-a", "v1", "-m", "v1"]);
    git(wt, &["checkout", "-q", "-b", "side"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "s1"]);
    git(wt, &["commit", "--allow-empty", "-q", "-m", "s2"]);
    git(wt, &["checkout", "-q", "main"]);
    git(wt, &["merge", "-q", "--no-ff", "side", "-m", "merge"]);

    let git_desc = git(wt, &["describe"]).trim().to_owned();
    let git_count = git(wt, &["rev-list", "--count", "v1..HEAD"])
        .trim()
        .parse::<usize>()
        .expect("count");

    let repo = Repository::open(
        &wt.join(".git"),
        &RepositoryOptions::empty().with_work_tree(wt),
    )
    .expect("open");
    let described = repo
        .rev_walk()
        .describe("HEAD", &DescribeOptions::default())
        .expect("describe")
        .expect("some tag");
    let tag_short = described
        .tag
        .as_str()
        .strip_prefix("refs/tags/")
        .unwrap_or(described.tag.as_str());
    assert_eq!(tag_short, "v1");
    assert_eq!(described.distance, git_count);
    assert_eq!(described.distance, 3);
    assert!(
        git_desc.starts_with("v1-3-g"),
        "git describe {git_desc}, grit distance {}",
        described.distance
    );
}

fn write_commit_at(
    odb: &grit_lib::objects::odb::Odb,
    parents: &[ObjectId],
    msg: &str,
    time: i64,
) -> grit_lib::error::Result<ObjectId> {
    let tree = odb.write_loose_materialize(ObjectKind::Tree, b"")?;
    let mut body = format!("tree {tree}\n");
    for p in parents {
        body.push_str(&format!("parent {p}\n"));
    }
    body.push_str(&format!(
        "author T <t@e.com> {time} +0000\ncommitter T <t@e.com> {time} +0000\n\n{msg}\n"
    ));
    odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
}

fn tag_annotated(repo: &Repository, name: &str, target: ObjectId, msg: &str) {
    let tagger = Signature::new("T", "t@e.com", repo.identity_now()).expect("tagger");
    let data = TagData {
        object: target,
        object_type: "commit".into(),
        tag: name.to_owned(),
        tagger: Some(tagger.header_line()),
        extra_headers: Vec::new(),
        message: format!("{msg}\n"),
        raw_message: None,
    };
    let bytes = serialize_tag(&data);
    let oid = repo.odb().write(ObjectKind::Tag, &bytes).expect("write");
    let full = format!("refs/tags/{name}");
    let name = RefName::try_from(full.as_str()).expect("name");
    repo.references().update(&name, &oid).expect("update");
}

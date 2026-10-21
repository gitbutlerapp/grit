//! Change ids as revision specs (prefix resolution via rev-parse).

use std::collections::HashSet;

use grit_lib::change_id::{find_commits_by_change_id_prefix, set_change_id, ChangeId};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{resolve_revision, resolve_revision_as_commit};
use grit_lib::revwalk::rev_parse_error::RevParseError;
use grit_test_support::git;

fn open_repo(dir: &std::path::Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
}

fn write_headerless_commit(
    repo: &Repository,
    parent: Option<grit_lib::objects::ObjectId>,
    message: &str,
) -> grit_lib::objects::ObjectId {
    let tree = parent
        .and_then(|p| {
            repo.odb()
                .read(&p)
                .ok()
                .and_then(|o| parse_commit(&o.data).ok())
        })
        .map(|c| c.tree)
        .unwrap_or_else(|| {
            grit_lib::objects::ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
                .unwrap()
        });
    let mut parents = Vec::new();
    if let Some(p) = parent {
        parents.push(p);
    }
    let data = CommitData {
        tree,
        parents,
        author: "T <t@example.com> 1 +0000".into(),
        committer: "T <t@example.com> 1 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let bytes = serialize_commit(&data);
    repo.odb()
        .write(ObjectKind::Commit, &bytes)
        .expect("write commit")
}

fn write_commit_with_id(
    repo: &Repository,
    parent: Option<grit_lib::objects::ObjectId>,
    message: &str,
    id: &ChangeId,
) -> grit_lib::objects::ObjectId {
    let tree = parent
        .and_then(|p| {
            repo.odb()
                .read(&p)
                .ok()
                .and_then(|o| parse_commit(&o.data).ok())
        })
        .map(|c| c.tree)
        .unwrap_or_else(|| {
            grit_lib::objects::ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
                .unwrap()
        });
    let mut parents = Vec::new();
    if let Some(p) = parent {
        parents.push(p);
    }
    let mut data = CommitData {
        tree,
        parents,
        author: "T <t@example.com> 1 +0000".into(),
        committer: "T <t@example.com> 1 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    set_change_id(&mut data, id);
    let bytes = serialize_commit(&data);
    repo.odb()
        .write(ObjectKind::Commit, &bytes)
        .expect("write commit")
}

#[test]
fn resolves_full_and_prefix_change_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-m",
            "root",
            "--author",
            "T <t@example.com>",
        ],
    );
    let repo = open_repo(dir.path());
    let root = resolve_revision(&repo, "HEAD").expect("head");
    let id = ChangeId::parse_reverse_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").expect("id");
    let tip = write_commit_with_id(&repo, Some(root), "tip", &id);
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &tip.to_hex()],
    );

    let full = resolve_revision(&repo, id.as_str()).expect("full id");
    assert_eq!(full, tip);
    let prefix = resolve_revision(&repo, &id.short(8)).expect("prefix");
    assert_eq!(prefix, tip);
}

#[test]
fn resolves_derived_id_of_headerless_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-m",
            "only",
            "--author",
            "T <t@example.com>",
        ],
    );
    let repo = open_repo(dir.path());
    let oid = resolve_revision(&repo, "HEAD").expect("head");
    let derived = ChangeId::derived_from_commit_id(&oid);
    assert!(derived.is_reverse_hex());
    let resolved = resolve_revision(&repo, derived.as_str()).expect("derived full");
    assert_eq!(resolved, oid);
    let resolved = resolve_revision(&repo, &derived.short(10)).expect("derived prefix");
    assert_eq!(resolved, oid);
}

#[test]
fn change_id_with_nav_suffix_and_range() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    for _ in 0..3 {
        git(
            dir.path(),
            &[
                "commit",
                "--allow-empty",
                "-m",
                "c",
                "--author",
                "T <t@example.com>",
            ],
        );
    }
    let repo = open_repo(dir.path());
    let parent = resolve_revision(&repo, "HEAD~1").expect("parent");
    let id = ChangeId::parse_reverse_hex("yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy").expect("id");
    let tip = write_commit_with_id(&repo, Some(parent), "marked", &id);
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &tip.to_hex()],
    );

    let parent_again = resolve_revision(&repo, &format!("{}~1", id.short(8))).expect("nav");
    assert_eq!(parent_again, parent);

    let range_tip =
        resolve_revision_as_commit(&repo, &format!("{}..main", id.short(8))).expect("range end");
    assert_eq!(range_tip, tip);
}

#[test]
fn refs_take_precedence_over_change_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-m",
            "root",
            "--author",
            "T <t@example.com>",
        ],
    );
    let repo = open_repo(dir.path());
    let root = resolve_revision(&repo, "HEAD").expect("head");
    let branch_id = ChangeId::parse_reverse_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").expect("id");
    let other_id = ChangeId::parse_reverse_hex("yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy").expect("id2");
    let branch_tip = write_commit_with_id(&repo, Some(root), "branch tip", &branch_id);
    let other = write_commit_with_id(&repo, Some(root), "other", &other_id);
    git(
        dir.path(),
        &["update-ref", "refs/heads/zzzz", &branch_tip.to_hex()],
    );
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &other.to_hex()],
    );

    let resolved = resolve_revision(&repo, "zzzz").expect("branch wins");
    assert_eq!(resolved, branch_tip);
}

#[test]
fn ambiguous_change_id_lists_all_candidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-m",
            "root",
            "--author",
            "T <t@example.com>",
        ],
    );
    let repo = open_repo(dir.path());
    let root = resolve_revision(&repo, "HEAD").expect("head");
    let shared = ChangeId::parse_reverse_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").expect("id");
    let a = write_commit_with_id(&repo, Some(root), "a", &shared);
    let b = write_commit_with_id(&repo, Some(root), "b", &shared);
    git(dir.path(), &["update-ref", "refs/heads/a", &a.to_hex()]);
    git(dir.path(), &["update-ref", "refs/heads/b", &b.to_hex()]);

    let err = resolve_revision(&repo, shared.as_str()).expect_err("ambiguous full");
    match err {
        Error::RevParse(RevParseError::AmbiguousChangeId { candidates, .. }) => {
            assert_eq!(candidates.len(), 2);
            let oids: Vec<_> = candidates.iter().map(|c| c.oid_hex.as_str()).collect();
            assert!(oids.contains(&a.to_hex().as_str()));
            assert!(oids.contains(&b.to_hex().as_str()));
        }
        other => panic!("expected AmbiguousChangeId, got {other:?}"),
    }
}

#[test]
fn ambiguous_change_id_prefix_lists_candidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-m",
            "root",
            "--author",
            "T <t@example.com>",
        ],
    );
    let repo = open_repo(dir.path());
    let root = resolve_revision(&repo, "HEAD").expect("head");
    let id_a = ChangeId::parse_reverse_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzy").expect("a id");
    let id_b = ChangeId::parse_reverse_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").expect("b id");
    let ca = write_commit_with_id(&repo, Some(root), "ca", &id_a);
    let cb = write_commit_with_id(&repo, Some(root), "cb", &id_b);
    git(dir.path(), &["update-ref", "refs/heads/c1", &ca.to_hex()]);
    git(dir.path(), &["update-ref", "refs/heads/c2", &cb.to_hex()]);

    let err = resolve_revision(&repo, "zzzzzzzzzzzzzzzz").expect_err("prefix ambiguous");
    match err {
        Error::RevParse(RevParseError::AmbiguousChangeId { candidates, .. }) => {
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected AmbiguousChangeId, got {other:?}"),
    }
}

#[test]
fn ambiguous_when_derived_prefix_collides_with_stored_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(
        dir.path(),
        &[
            "commit",
            "--allow-empty",
            "-m",
            "root",
            "--author",
            "T <t@example.com>",
        ],
    );
    let repo = open_repo(dir.path());
    let root = resolve_revision(&repo, "HEAD").expect("head");
    let headerless = write_headerless_commit(&repo, Some(root), "headerless");
    let derived = ChangeId::derived_from_commit_id(&headerless);
    let prefix = derived.short(4);
    assert!(
        grit_lib::change_id::is_change_id_spec_token(&prefix),
        "test prefix must be a valid change-id token"
    );
    let mut stored_id_str = prefix.to_string();
    while stored_id_str.len() < 32 {
        stored_id_str.push('z');
    }
    let stored_id = ChangeId::parse_reverse_hex(&stored_id_str).expect("stored id");
    assert_ne!(
        ChangeId::derived_from_commit_id(&headerless),
        stored_id,
        "stored id must not equal headerless derived id"
    );
    let with_header = write_commit_with_id(&repo, Some(root), "stored header", &stored_id);
    git(
        dir.path(),
        &["update-ref", "refs/heads/headerless", &headerless.to_hex()],
    );
    git(
        dir.path(),
        &["update-ref", "refs/heads/stored", &with_header.to_hex()],
    );

    let matches = find_commits_by_change_id_prefix(&repo, &prefix).expect("lookup");
    assert_eq!(
        matches.len(),
        2,
        "prefix {prefix} must match headerless derived and stored header"
    );
    let oids: HashSet<_> = matches.iter().map(|m| m.oid).collect();
    assert!(oids.contains(&headerless));
    assert!(oids.contains(&with_header));

    let err = resolve_revision(&repo, &prefix).expect_err("ambiguous");
    match err {
        Error::RevParse(RevParseError::AmbiguousChangeId { candidates, .. }) => {
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected AmbiguousChangeId, got {other:?}"),
    }
}

#[test]
fn short_or_non_reverse_hex_specs_unaffected() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("tmp"), "x").expect("file");
    git(dir.path(), &["add", "tmp"]);
    git(
        dir.path(),
        &["commit", "-m", "track tmp", "--author", "T <t@example.com>"],
    );
    let repo = open_repo(dir.path());
    assert!(resolve_revision(&repo, "tmp").is_err());
    assert!(resolve_revision(&repo, "dead").is_err());
    assert!(resolve_revision(&repo, "abc").is_err());
}

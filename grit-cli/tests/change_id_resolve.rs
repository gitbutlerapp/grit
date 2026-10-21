//! CLI: resolve and show accept change-id revision specs.

use grit_lib::change_id::{set_change_id, ChangeId};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectKind};
use grit_lib::repo::Repository;
use grit_test_support::{git, grit};

fn seed_repo_with_change_id() -> (tempfile::TempDir, ChangeId, String) {
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
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let root_hex = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();
    let root = root_hex.parse().expect("oid");
    let id = ChangeId::parse_reverse_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").expect("id");
    let tree = parse_commit(&repo.odb().read(&root).expect("read").data)
        .expect("parse")
        .tree;
    let mut data = CommitData {
        tree,
        parents: vec![root],
        author: "T <t@example.com> 1 +0000".into(),
        committer: "T <t@example.com> 1 +0000".into(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "tip\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    set_change_id(&mut data, &id);
    let tip = repo
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&data))
        .expect("write");
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &tip.to_hex()],
    );
    (dir, id, tip.to_hex())
}

#[test]
fn show_json_resolves_change_id_prefix() {
    let (dir, id, tip_hex) = seed_repo_with_change_id();
    let prefix = id.short(8);
    let out = grit(dir.path(), &["show", prefix, "--json"]);
    assert!(out.ok(), "{}", out.dump("show"));
    let v: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    let oid = v
        .pointer("/commit/oid")
        .and_then(|x| x.as_str())
        .expect("commit.oid");
    assert_eq!(oid, tip_hex);
}

#[test]
fn resolve_ambiguous_change_id_exits_nonzero() {
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
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let root_hex = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();
    let root = root_hex.parse().expect("oid");
    let shared = ChangeId::parse_reverse_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").expect("id");
    let tree = parse_commit(&repo.odb().read(&root).expect("read").data)
        .expect("parse")
        .tree;
    for (name, msg) in [("a", "a"), ("b", "b")] {
        let mut data = CommitData {
            tree,
            parents: vec![root],
            author: "T <t@example.com> 1 +0000".into(),
            committer: "T <t@example.com> 1 +0000".into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("{msg}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        set_change_id(&mut data, &shared);
        let oid = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&data))
            .expect("write");
        git(
            dir.path(),
            &["update-ref", &format!("refs/heads/{name}"), &oid.to_hex()],
        );
    }
    let out = grit(dir.path(), &["resolve", shared.as_str()]);
    assert!(!out.ok());
    let stderr = &out.stderr;
    assert!(stderr.contains("ambiguous change id"), "stderr={stderr}");
    assert!(stderr.contains("zzzzzzzz"));
}

//! [`CommitRef`] round-trip against commits built in a fixture repository.

use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::objects::{parse_commit, serialize_commit, CommitRef, ObjectId, ObjectKind};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_test_support::objects::{HashAlgo as FixtureAlgo, RepoFixture};

fn git_hash_commit(repo: &RepoFixture, body: &[u8]) -> ObjectId {
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["hash-object", "-w", "-t", "commit", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hash-object");
    child.stdin.take().unwrap().write_all(body).expect("stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("commit oid")
}

#[test]
fn commit_ref_roundtrip() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let repo = Repository::discover(Some(fixture.path()), &RepositoryOptions::empty()).expect("open");
    let empty = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    let samples: Vec<Vec<u8>> = vec![
        format!(
            "tree {empty}\nauthor A <a@example.com> 1 +0000\ncommitter C <c@example.com> 1 +0000\n\nplain\n"
        )
        .into_bytes(),
        {
            let mut b = format!(
                "tree {empty}\nauthor A <a@example.com> 1 +0000\ncommitter C <c@example.com> 1 +0000\nencoding ISO-8859-1\n\ncaf"
            )
            .into_bytes();
            b.push(0xe9);
            b.push(b'\n');
            b
        },
        format!(
            "tree {empty}\nauthor A <a@example.com> 1 +0000\ncommitter C <c@example.com> 1 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n sig\n -----END PGP SIGNATURE-----\nchange-id fixture-change\n\nsigned\n"
        )
        .into_bytes(),
        format!(
            "tree {empty}\nauthor A <a@example.com> 1 +0000\ncommitter C <c@example.com> 1 +0000\nmergetag object {empty}\n type commit\n tag t\n tagger T <t@e.com> 1 +0000\n\nmerge-tag-body\n"
        )
        .into_bytes(),
    ];

    for body in &samples {
        git_hash_commit(&fixture, body);
    }

    let _root = fixture
        .git(&["commit-tree", empty, "-m", "root"])
        .stdout
        .trim()
        .to_string();
    fixture.git(&["commit-tree", empty, "-p", &_root, "-m", "child"]);

    let rev_list = fixture.git(&["rev-list", "--all"]).stdout;
    for line in rev_list.lines() {
        let oid: ObjectId = line.trim().parse().expect("rev-list oid");
        let obj = repo.odb().read(&oid).expect("read");
        assert_eq!(obj.kind, ObjectKind::Commit, "expected commit {oid}");
        let raw = &obj.data;
        let legacy = parse_commit(raw).expect("parse_commit");
        let cref = CommitRef::parse(raw).expect("CommitRef::parse");
        assert_eq!(cref.to_owned(), legacy, "to_owned mismatch for {oid}");
        assert_eq!(serialize_commit(&legacy), *raw, "legacy serialize for {oid}");
        assert_eq!(cref.to_bytes(), *raw, "CommitRef serialize for {oid}");
    }
}

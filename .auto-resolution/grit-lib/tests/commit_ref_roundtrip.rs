//! [`CommitRef`] round-trip against commits built in a fixture repository.

use std::io::Write;
use std::process::{Command, Stdio};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, CommitRef, ObjectId, ObjectKind};
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
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("commit oid")
}

#[test]
fn commit_ref_roundtrip() {
    let fixture = RepoFixture::init(FixtureAlgo::Sha1).expect("init");
    let repo =
        Repository::discover(Some(fixture.path()), &RepositoryOptions::empty()).expect("open");
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
        format!(
            "tree {empty}\nauthor A <a@example.com> 1 +0000\ncommitter C <c@example.com> 1 +0000\nchange-id interleaved\nencoding UTF-8\ngpgsig -----BEGIN PGP SIGNATURE-----\n sig\n -----END PGP SIGNATURE-----\n\ninterleaved-body\n"
        )
        .into_bytes(),
    ];

    let mut sample_oids = Vec::with_capacity(samples.len());
    for body in &samples {
        sample_oids.push(git_hash_commit(&fixture, body));
    }

    let root = fixture
        .git(&["commit-tree", empty, "-m", "root"])
        .stdout
        .trim()
        .to_string();
    let child: ObjectId = fixture
        .git(&["commit-tree", empty, "-p", &root, "-m", "child"])
        .stdout
        .trim()
        .parse()
        .expect("child oid");

    let mut oids: Vec<ObjectId> = sample_oids;
    oids.push(child);

    for oid in oids {
        let obj = repo.odb().read(&oid).expect("read");
        assert_eq!(obj.kind, ObjectKind::Commit, "expected commit {oid}");
        let raw = &obj.data;
        let legacy = parse_commit(raw).expect("parse_commit");
        let cref = CommitRef::parse(raw).expect("CommitRef::parse");
        assert_eq!(cref.to_owned(), legacy, "to_owned mismatch for {oid}");
        assert_eq!(cref.to_bytes(), *raw, "CommitRef serialize for {oid}");
    }
}

//! Notes round-trips and fanout updates checked against system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::notes::{read_notes_tree, resolve_configured_notes_ref, NotesRef};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::refs::store::RefName;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::{NoteAddOutcome, NotesWriteOptions};
use grit_test_support::git;

const IDENT: &str = "Notes Test <notes@grit-scm.com> 1700000000 +0000";

fn git_cmd(root: &std::path::Path, args: &[&str]) -> String {
    git(root, args)
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

fn open_grit(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn assert_fsck_strict(root: &std::path::Path) {
    let out = git_cmd(root, &["fsck", "--strict"]);
    assert!(
        !out.to_lowercase().contains("error"),
        "fsck --strict must be clean:\n{out}"
    );
}

fn seed_commit(root: &std::path::Path, path: &str, content: &str, msg: &str) -> ObjectId {
    std::fs::write(root.join(path), content).expect("write");
    git_cmd(root, &["add", path]);
    git_cmd(root, &["commit", "-qm", msg]);
    git_cmd(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("HEAD")
}

fn write_opts(message: &str) -> NotesWriteOptions {
    NotesWriteOptions {
        message: message.into(),
        committer: IDENT.parse().expect("signature"),
        author: None,
    }
}

#[test]
fn grit_add_git_show() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "f", "body\n", "init");

    let repo = open_grit(dir.path());
    repo.notes()
        .add(
            &NotesRef::commits(),
            &commit,
            b"grit note\n",
            &write_opts("add note"),
            false,
        )
        .expect("add");

    assert_fsck_strict(dir.path());
    assert_eq!(
        git_cmd(dir.path(), &["notes", "show", &commit.to_hex()]).trim(),
        "grit note"
    );
}

#[test]
fn git_add_grit_show() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "f", "body\n", "init");

    git_cmd(
        dir.path(),
        &["notes", "add", "-m", "git note", &commit.to_hex()],
    );

    let repo = open_grit(dir.path());
    let body = repo
        .notes()
        .read(&NotesRef::commits(), &commit)
        .expect("read")
        .expect("note bytes");
    assert_eq!(body, b"git note\n");
}

#[test]
fn fanout_notes_tree_read_and_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    git_cmd(dir.path(), &["config", "user.email", "t@example.com"]);

    std::fs::write(dir.path().join("base"), "b\n").expect("write");
    git_cmd(dir.path(), &["add", "base"]);
    git_cmd(dir.path(), &["commit", "-qm", "root"]);

    let mut sample_commits: Vec<ObjectId> = Vec::new();
    for i in 0..320 {
        std::fs::write(dir.path().join("f"), format!("v{i}\n")).expect("write");
        git_cmd(dir.path(), &["add", "f"]);
        git_cmd(dir.path(), &["commit", "-qm", &format!("c{i}")]);
        git_cmd(
            dir.path(),
            &["notes", "add", "-m", &format!("note-{i}"), "HEAD"],
        );
        if i == 10 || i == 200 || i == 319 {
            let head: ObjectId = git_cmd(dir.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("HEAD");
            sample_commits.push(head);
        }
    }
    assert_fsck_strict(dir.path());

    let repo = open_grit(dir.path());
    let entries = read_notes_tree(&repo, "refs/notes/commits").expect("read fanout");
    assert!(
        entries.iter().any(|e| e.path.iter().any(|b| *b == b'/')),
        "git must use fanout paths with 320 notes"
    );

    let add_target = sample_commits[0];
    repo.notes()
        .add(
            &NotesRef::commits(),
            &add_target,
            b"grit replaced fanout note\n",
            &write_opts("rewrite in fanout tree"),
            true,
        )
        .expect("force add in fanout");

    let remove_target = sample_commits[1];
    repo.notes()
        .remove(
            &NotesRef::commits(),
            &remove_target,
            &write_opts("remove in fanout tree"),
        )
        .expect("remove in fanout");

    assert_fsck_strict(dir.path());
    assert_eq!(
        git_cmd(dir.path(), &["notes", "show", &add_target.to_hex()]).trim(),
        "grit replaced fanout note"
    );
    let listed = git_cmd(dir.path(), &["notes", "list"]);
    assert!(
        listed.contains(&add_target.to_hex()),
        "git notes list must still include rewritten note"
    );
    assert!(
        !listed.contains(&remove_target.to_hex()),
        "git notes list must not include removed note"
    );

    let note_lines: Vec<_> = listed.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        note_lines.len(),
        319,
        "expected 319 notes after removing one from 320"
    );

    let untouched = sample_commits[2];
    assert_eq!(
        git_cmd(dir.path(), &["notes", "show", &untouched.to_hex()]).trim(),
        "note-319",
        "unaffected fanout note body must match git"
    );
}

#[test]
fn add_existing_without_force_is_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "f", "x\n", "init");
    git_cmd(
        dir.path(),
        &["notes", "add", "-m", "existing", &commit.to_hex()],
    );

    let repo = open_grit(dir.path());
    let err = repo
        .notes()
        .add(
            &NotesRef::commits(),
            &commit,
            b"new\n",
            &write_opts("add"),
            false,
        )
        .expect_err("must reject duplicate note");
    assert!(
        matches!(err, Error::NoteAlreadyExists { .. }),
        "expected typed NoteAlreadyExists, got {err:?}"
    );
}

#[test]
fn padded_git_notes_ref_is_invalid() {
    let err = NotesRef::from_full(" refs/notes/spaced ").expect_err("padded ref");
    assert!(matches!(err, Error::InvalidNotesRef(_)));

    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "f", "x\n", "init");
    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .env("GIT_NOTES_REF", " refs/notes/spaced ")
        .args(["notes", "add", "-m", "test", &commit.to_hex()])
        .output()
        .expect("git notes");
    assert!(
        !git_out.status.success(),
        "system git must reject padded GIT_NOTES_REF"
    );
}

#[test]
fn trailing_space_notes_ref_is_invalid() {
    let err = NotesRef::from_full("refs/notes/spaced ").expect_err("trailing space");
    assert!(matches!(err, Error::InvalidNotesRef(_)));
}

#[test]
fn padded_core_notes_ref_is_invalid() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    git_cmd(
        dir.path(),
        &["config", "core.notesRef", " refs/notes/spaced "],
    );
    let repo = open_grit(dir.path());
    let err =
        resolve_configured_notes_ref(repo.environment(), repo.config().expect("config").as_ref())
            .expect_err("padded core.notesRef");
    assert!(
        matches!(err, Error::InvalidNotesRef(_)),
        "expected InvalidNotesRef, got {err:?}"
    );
}

#[test]
fn empty_core_notes_ref_is_invalid() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    git_cmd(dir.path(), &["config", "core.notesRef", ""]);
    let repo = open_grit(dir.path());
    let err =
        resolve_configured_notes_ref(repo.environment(), repo.config().expect("config").as_ref())
            .expect_err("empty config ref");
    assert!(
        matches!(err, Error::InvalidNotesRef(_)),
        "expected InvalidNotesRef, got {err:?}"
    );
}

#[test]
fn custom_notes_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    git_cmd(
        dir.path(),
        &["config", "core.notesRef", "refs/notes/custom"],
    );
    let commit = seed_commit(dir.path(), "f", "x\n", "init");

    let repo = open_grit(dir.path());
    let notes_ref =
        resolve_configured_notes_ref(repo.environment(), repo.config().expect("config").as_ref())
            .expect("resolve");
    assert_eq!(notes_ref.as_str(), "refs/notes/custom");

    repo.notes()
        .add(
            &notes_ref,
            &commit,
            b"custom ref note\n",
            &write_opts("add on custom ref"),
            false,
        )
        .expect("add");

    assert_eq!(
        git_cmd(
            dir.path(),
            &["notes", "--ref=refs/notes/custom", "show", &commit.to_hex()],
        )
        .trim(),
        "custom ref note"
    );
    assert_fsck_strict(dir.path());
}

#[test]
fn list_with_bodies_matches_git_notes() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let first = seed_commit(dir.path(), "f", "1\n", "first");
    let second = seed_commit(dir.path(), "f", "2\n", "second");
    git_cmd(dir.path(), &["notes", "add", "-m", "one", &first.to_hex()]);
    git_cmd(
        dir.path(),
        &["notes", "add", "-m", "two\n\nmore", &second.to_hex()],
    );

    let repo = open_grit(dir.path());
    let notes = repo
        .notes()
        .list_with_bodies(&NotesRef::commits(), None)
        .expect("list with bodies")
        .items;

    let mut expected: Vec<(ObjectId, ObjectId)> = git_cmd(dir.path(), &["notes", "list"])
        .lines()
        .map(|line| {
            let (blob, object) = line.split_once(' ').expect("blob and object");
            (object.parse().expect("object"), blob.parse().expect("blob"))
        })
        .collect();
    expected.sort();
    let listed: Vec<(ObjectId, ObjectId)> = notes.iter().map(|n| (n.object, n.note_blob)).collect();
    assert_eq!(listed, expected, "objects, blobs and order must match git");
    for note in &notes {
        assert_eq!(
            String::from_utf8_lossy(&note.body),
            git_cmd(dir.path(), &["notes", "show", &note.object.to_hex()]),
            "note body must match git notes show"
        );
    }
}

#[test]
fn list_with_bodies_reports_missing_note_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "f", "x\n", "init");
    let missing_blob = "1111111111111111111111111111111111111111";

    let mut mktree = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["mktree", "--missing"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn mktree");
    {
        use std::io::Write;
        let stdin = mktree.stdin.as_mut().expect("stdin");
        writeln!(stdin, "100644 blob {missing_blob}\t{}", commit.to_hex()).expect("tree line");
    }
    let tree =
        String::from_utf8(mktree.wait_with_output().expect("mktree").stdout).expect("utf8 tree id");
    let notes_commit = git_cmd(dir.path(), &["commit-tree", "-m", "broken", tree.trim()]);
    git_cmd(
        dir.path(),
        &["update-ref", "refs/notes/commits", notes_commit.trim()],
    );

    let repo = open_grit(dir.path());
    assert_eq!(
        repo.notes()
            .list(&NotesRef::commits(), None)
            .expect("list")
            .items
            .len(),
        1,
        "listing without bodies only reads the tree"
    );
    let err = repo
        .notes()
        .list_with_bodies(&NotesRef::commits(), None)
        .expect_err("a missing note blob must not read as an empty note");
    assert!(
        matches!(err, Error::ObjectNotFound(_)),
        "expected ObjectNotFound for the note blob, got {err:?}"
    );
}

#[test]
fn add_reports_added_then_replaced_and_logs_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "f", "x\n", "init");

    let repo = open_grit(dir.path());
    let notes = repo.notes();
    let first = notes
        .add(
            &NotesRef::commits(),
            &commit,
            b"first\n",
            &write_opts("Notes added by grit"),
            false,
        )
        .expect("add");
    assert_eq!(first, NoteAddOutcome::Added);
    let second = notes
        .add(
            &NotesRef::commits(),
            &commit,
            b"second\n",
            &write_opts("Notes replaced by grit"),
            true,
        )
        .expect("force add");
    assert_eq!(second, NoteAddOutcome::Replaced);

    assert_fsck_strict(dir.path());
    assert_eq!(
        git_cmd(dir.path(), &["notes", "show", &commit.to_hex()]),
        "second\n"
    );
    let reflog = git_cmd(
        dir.path(),
        &["reflog", "show", "--format=%gs", "refs/notes/commits"],
    );
    assert_eq!(
        reflog.lines().collect::<Vec<_>>(),
        vec![
            "notes: Notes replaced by grit",
            "notes: Notes added by grit"
        ],
        "git must read the notes reflog grit wrote"
    );
    let tip = git_cmd(dir.path(), &["rev-parse", "refs/notes/commits"]);
    let parent = git_cmd(dir.path(), &["rev-parse", "refs/notes/commits^"]);
    let entries = repo
        .references()
        .reflog(&RefName::try_from("refs/notes/commits").expect("ref name"))
        .expect("grit reflog");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].new_oid.to_hex(), tip.trim());
    assert_eq!(entries[1].old_oid.to_hex(), parent.trim());
    assert!(entries[0].old_oid.is_zero());
}

#[test]
fn notes_on_reftable_repository_use_the_ref_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .expect("init reftable");
    let commit = create_commit(
        &repo,
        &CommitRequest {
            message: "init".into(),
            author: Some(IDENT.parse().expect("signature")),
            committer: Some(IDENT.parse().expect("signature")),
            allow_empty: true,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        },
        &mut NullProgress,
    )
    .expect("commit")
    .oid;

    let notes_ref = NotesRef::parse("review");
    let notes = repo.notes();
    assert_eq!(
        notes
            .add(&notes_ref, &commit, b"lgtm\n", &write_opts("review"), false)
            .expect("add on reftable"),
        NoteAddOutcome::Added
    );
    assert_eq!(
        notes.read(&notes_ref, &commit).expect("read"),
        Some(b"lgtm\n".to_vec())
    );
    let listed = notes
        .list_with_bodies(&notes_ref, None)
        .expect("list")
        .items;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].object, commit);
    assert_eq!(listed[0].body, b"lgtm\n");

    let tip = repo
        .references()
        .resolve(&RefName::try_from("refs/notes/review").expect("ref name"))
        .expect("notes ref in reftable");
    assert!(
        !repo.git_dir().join("refs/notes/review").exists(),
        "a reftable repository must not grow a loose notes ref"
    );
    assert_eq!(
        notes
            .remove_many(&notes_ref, &[commit], &write_opts("drop"))
            .expect("remove"),
        vec![commit]
    );
    let after = repo
        .references()
        .resolve(&RefName::try_from("refs/notes/review").expect("ref name"))
        .expect("notes ref after remove");
    let parents = grit_lib::objects::parse_commit(&repo.odb().read(&after).expect("read").data)
        .expect("parse")
        .parents;
    assert_eq!(parents, vec![tip]);
    assert!(notes.list(&notes_ref, None).expect("list").items.is_empty());
}

//! Notes add/remove planning without persisting repository state.

use grit_lib::notes::NotesRef;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{NoteAddOutcome, NotesWriteOptions};
use grit_test_support::{git, git_dir_state_digest, git_fsck};
use tempfile::TempDir;

fn open_repo(path: &std::path::Path) -> Repository {
    Repository::discover(
        Some(path),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("repo")
}

fn init_with_commit() -> (TempDir, grit_lib::objects::ObjectId) {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), "a\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            "init",
        ],
    );
    let oid = git(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid");
    (dir, oid)
}

fn write_opts() -> NotesWriteOptions {
    NotesWriteOptions::new(
        "add note",
        "Test <t@example.com> 1700000000 +0000"
            .parse()
            .expect("signature"),
    )
}

#[test]
fn notes_plan_add_writes_nothing() {
    let (dir, object) = init_with_commit();
    let git_dir = dir.path().join(".git");
    let before = git_dir_state_digest(&git_dir);
    let repo = open_repo(dir.path());
    let plan = repo
        .notes()
        .plan_add(
            &NotesRef::commits(),
            &object,
            b"hello\n",
            &write_opts(),
            false,
        )
        .expect("plan");
    assert_eq!(plan.outcome, NoteAddOutcome::Added);
    assert!(plan.notes_ref_tip_before.is_none());
    assert_eq!(before, git_dir_state_digest(&git_dir));
    let fsck = git_fsck(dir.path(), true);
    assert!(fsck.ok, "fsck: {:?}", fsck.msg_ids);
}

fn rev_parse(dir: &std::path::Path, rev: &str) -> grit_lib::objects::ObjectId {
    git(dir, &["rev-parse", rev]).trim().parse().expect("oid")
}

/// Repository with two commits, each carrying a note written by `git notes`.
fn init_with_two_notes() -> (
    TempDir,
    grit_lib::objects::ObjectId,
    grit_lib::objects::ObjectId,
) {
    let (dir, first) = init_with_commit();
    std::fs::write(dir.path().join("b.txt"), "b\n").expect("write");
    git(dir.path(), &["add", "b.txt"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            "second",
        ],
    );
    let second = rev_parse(dir.path(), "HEAD");
    for (object, body) in [(first, "first"), (second, "second")] {
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=t@example.com",
                "notes",
                "add",
                "-m",
                body,
                &object.to_hex(),
            ],
        );
    }
    (dir, first, second)
}

#[test]
fn notes_plan_remove_many_predicts_remove_many() {
    let (dir, first, second) = init_with_two_notes();
    let unnoted = rev_parse(dir.path(), "HEAD:a.txt");
    let tip_before = rev_parse(dir.path(), "refs/notes/commits");
    let git_dir = dir.path().join(".git");
    let before = git_dir_state_digest(&git_dir);
    let repo = open_repo(dir.path());
    let objects = [second, unnoted, first, second];

    let plan = repo
        .notes()
        .plan_remove_many(&NotesRef::commits(), &objects, &write_opts())
        .expect("plan");
    assert_eq!(plan.removed, vec![second, first]);
    assert_eq!(plan.notes_ref_tip_before, Some(tip_before));
    assert_eq!(before, git_dir_state_digest(&git_dir));

    let removed = repo
        .notes()
        .remove_many(&NotesRef::commits(), &objects, &write_opts())
        .expect("remove");
    assert_eq!(removed, plan.removed);
    let tip_after = rev_parse(dir.path(), "refs/notes/commits");
    assert_eq!(plan.notes_ref_tip_after, Some(tip_after));
    assert_eq!(rev_parse(dir.path(), "refs/notes/commits~1"), tip_before);
    assert_eq!(git(dir.path(), &["notes", "list"]).trim(), "");
    let fsck = git_fsck(dir.path(), true);
    assert!(fsck.ok, "fsck: {:?}", fsck.msg_ids);
}

#[test]
fn notes_plan_remove_many_without_notes_changes_nothing() {
    let (dir, object) = init_with_commit();
    let repo = open_repo(dir.path());
    let plan = repo
        .notes()
        .plan_remove_many(&NotesRef::commits(), &[object], &write_opts())
        .expect("plan");
    assert!(plan.removed.is_empty());
    assert!(plan.notes_ref_tip_before.is_none());
    assert!(plan.notes_ref_tip_after.is_none());
}

#[test]
fn notes_plan_remove_agrees_with_plan_remove_many() {
    let (dir, first, _second) = init_with_two_notes();
    let repo = open_repo(dir.path());
    let single = repo
        .notes()
        .plan_remove(&NotesRef::commits(), &first, &write_opts())
        .expect("plan");
    let many = repo
        .notes()
        .plan_remove_many(&NotesRef::commits(), &[first], &write_opts())
        .expect("plan many");
    assert!(single.had_note);
    assert_eq!(single.object, first);
    assert_eq!(many.removed, vec![first]);
    assert_eq!(single.notes_ref_tip_before, many.notes_ref_tip_before);
    assert_eq!(single.notes_ref_tip_after, many.notes_ref_tip_after);
}

#[test]
fn notes_plan_remove_missing_note() {
    let (dir, object) = init_with_commit();
    let git_dir = dir.path().join(".git");
    let before = git_dir_state_digest(&git_dir);
    let repo = open_repo(dir.path());
    let plan = repo
        .notes()
        .plan_remove(&NotesRef::commits(), &object, &write_opts())
        .expect("plan");
    assert!(!plan.had_note);
    assert!(plan.notes_ref_tip_after.is_none());
    assert_eq!(before, git_dir_state_digest(&git_dir));
}

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
    NotesWriteOptions {
        message: "add note".into(),
        committer: "Test <t@example.com> 1700000000 +0000"
            .parse()
            .expect("signature"),
        author: None,
    }
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

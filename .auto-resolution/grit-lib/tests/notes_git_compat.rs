//! Notes tree read/write/merge round-trips with system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::notes::{
    notes_merge_inner, read_notes_tree, upsert_note_entry, write_notes_commit, NotesMergeStrategy,
};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_test_support::git;

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

fn git_note_show(root: &std::path::Path, object: &ObjectId) -> String {
    git_cmd(root, &["notes", "show", &object.to_hex()])
}

fn grit_note_blob(repo: &Repository, notes_ref: &str, object: &ObjectId) -> Option<String> {
    let entries = read_notes_tree(repo, notes_ref).ok()?;
    let hex = object.to_hex();
    let entry = entries
        .iter()
        .find(|e| grit_lib::notes::note_object_name(&e.path).as_deref() == Some(hex.as_str()))?;
    let blob = repo.odb().read(&entry.oid).ok()?;
    String::from_utf8(blob.data).ok()
}

fn write_grit_note(repo: &Repository, object: &ObjectId, text: &str, message: &str) {
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, text.as_bytes())
        .expect("blob");
    let mut entries = read_notes_tree(repo, "refs/notes/commits").unwrap_or_default();
    upsert_note_entry(&mut entries, &object.to_hex(), blob);
    write_notes_commit(repo, "refs/notes/commits", &entries, message).expect("write notes");
}

#[test]
fn t3301_notes_roundtrip_with_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "f", "body\n", "init");

    write_grit_note(&open_grit(dir.path()), &commit, "grit note\n", "add note");
    assert_fsck_strict(dir.path());
    assert_eq!(
        git_note_show(dir.path(), &commit).trim(),
        "grit note",
        "git notes show must read grit-written note"
    );

    git_cmd(
        dir.path(),
        &["notes", "add", "-f", "-m", "git note", &commit.to_hex()],
    );
    let repo = open_grit(dir.path());
    assert_eq!(
        grit_note_blob(&repo, "refs/notes/commits", &commit).as_deref(),
        Some("git note\n"),
        "grit must read git-written note"
    );

    let listed = git_cmd(dir.path(), &["notes", "list"]);
    assert!(
        listed.contains(&commit.to_hex()[..7]),
        "git notes list should show annotated commit: {listed}"
    );
}

#[test]
fn t3305_fanout_layouts_read_and_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("base"), "b\n").expect("write");
    git_cmd(dir.path(), &["add", "base"]);
    git_cmd(dir.path(), &["commit", "-qm", "root"]);

    for i in 0..260 {
        std::fs::write(dir.path().join("f"), format!("v{i}\n")).expect("write");
        git_cmd(dir.path(), &["add", "f"]);
        git_cmd(dir.path(), &["commit", "-qm", &format!("c{i}")]);
        git_cmd(
            dir.path(),
            &["notes", "add", "-m", &format!("note-{i}"), "HEAD"],
        );
    }
    assert_fsck_strict(dir.path());

    let repo = open_grit(dir.path());
    let entries = read_notes_tree(&repo, "refs/notes/commits").expect("read git fanout");
    assert!(
        entries.iter().any(|e| e.path.iter().any(|b| *b == b'/')),
        "git fanout layout must use slash-separated paths"
    );

    let head: ObjectId = git_cmd(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("HEAD");
    write_grit_note(&repo, &head, "grit fanout rewrite\n", "rewrite one");
    assert_fsck_strict(dir.path());
    assert_eq!(
        git_note_show(dir.path(), &head).trim(),
        "grit fanout rewrite",
        "git must read grit fanout write"
    );
}

#[test]
fn t3309_notes_merge_strategies_match_git() {
    let strategies = [
        (NotesMergeStrategy::Theirs, "theirs"),
        (NotesMergeStrategy::Ours, "ours"),
        (NotesMergeStrategy::Union, "union"),
        (NotesMergeStrategy::CatSortUniq, "cat_sort_uniq"),
    ];

    for (grit_strategy, git_strategy) in strategies {
        let grit_dir = tempfile::tempdir().expect("grit temp");
        let git_dir = tempfile::tempdir().expect("git temp");
        for dir in [grit_dir.path(), git_dir.path()] {
            init_repo(dir);
            let commit = seed_commit(dir, "x", "x\n", "seed");
            git_cmd(dir, &["notes", "add", "-m", "base", &commit.to_hex()]);
            let base = git_cmd(dir, &["rev-parse", "refs/notes/commits"])
                .trim()
                .to_owned();
            git_cmd(
                dir,
                &["notes", "add", "-f", "-m", "local", &commit.to_hex()],
            );
            let local_tip = git_cmd(dir, &["rev-parse", "refs/notes/commits"])
                .trim()
                .to_owned();
            git_cmd(dir, &["update-ref", "refs/notes/remote", &base]);
            git_cmd(
                dir,
                &[
                    "notes",
                    "--ref=refs/notes/remote",
                    "add",
                    "-f",
                    "-m",
                    "remote",
                    &commit.to_hex(),
                ],
            );
            git_cmd(dir, &["update-ref", "refs/notes/commits", &local_tip]);
        }

        git_cmd(
            git_dir.path(),
            &["notes", "merge", "-s", git_strategy, "refs/notes/remote"],
        );

        let grit_repo = open_grit(grit_dir.path());
        notes_merge_inner(
            &grit_repo,
            "refs/notes/commits",
            "refs/notes/remote",
            grit_strategy,
        )
        .expect("merge")
        .expect("no conflict");

        let commit: ObjectId = git_cmd(grit_dir.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("HEAD");
        assert_eq!(
            git_note_show(grit_dir.path(), &commit).trim(),
            git_note_show(git_dir.path(), &commit).trim(),
            "notes merge strategy {git_strategy} must match git note text"
        );
        assert_fsck_strict(grit_dir.path());
    }
}

#[test]
fn t3309_manual_notes_merge_keeps_local_ref_on_conflict() {
    let grit_dir = tempfile::tempdir().expect("grit temp");
    let git_dir = tempfile::tempdir().expect("git temp");
    for dir in [grit_dir.path(), git_dir.path()] {
        init_repo(dir);
        let commit = seed_commit(dir, "x", "x\n", "seed");
        git_cmd(dir, &["notes", "add", "-m", "base", &commit.to_hex()]);
        let base = git_cmd(dir, &["rev-parse", "refs/notes/commits"])
            .trim()
            .to_owned();
        git_cmd(
            dir,
            &["notes", "add", "-f", "-m", "local", &commit.to_hex()],
        );
        let local_tip = git_cmd(dir, &["rev-parse", "refs/notes/commits"])
            .trim()
            .to_owned();
        git_cmd(dir, &["update-ref", "refs/notes/remote", &base]);
        git_cmd(
            dir,
            &[
                "notes",
                "--ref=refs/notes/remote",
                "add",
                "-f",
                "-m",
                "remote",
                &commit.to_hex(),
            ],
        );
        git_cmd(dir, &["update-ref", "refs/notes/commits", &local_tip]);
    }

    let _ = grit_test_support::git_cmd(&["notes", "merge", "-s", "manual", "refs/notes/remote"])
        .in_dir(git_dir.path())
        .with_status(1);
    let git_notes_tip = git_cmd(git_dir.path(), &["rev-parse", "refs/notes/commits"])
        .trim()
        .to_owned();

    let grit_repo = open_grit(grit_dir.path());
    let local_tip = git_cmd(grit_dir.path(), &["rev-parse", "refs/notes/commits"])
        .trim()
        .to_owned();
    notes_merge_inner(
        &grit_repo,
        "refs/notes/commits",
        "refs/notes/remote",
        NotesMergeStrategy::Manual,
    )
    .expect("merge")
    .expect_err("manual merge must report conflicts");

    let grit_notes_tip = git_cmd(grit_dir.path(), &["rev-parse", "refs/notes/commits"])
        .trim()
        .to_owned();
    assert_eq!(
        grit_notes_tip, local_tip,
        "grit must keep refs/notes/commits at the local tip during manual conflicts"
    );
    assert_eq!(
        grit_notes_tip, git_notes_tip,
        "grit notes ref must match git after manual merge conflict"
    );
    assert_fsck_strict(grit_dir.path());
}

#[test]
fn t3303_notes_subtrees_read_flat_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let c1 = seed_commit(dir.path(), "a", "1\n", "one");
    let c2 = seed_commit(dir.path(), "b", "2\n", "two");
    git_cmd(dir.path(), &["notes", "add", "-m", "n1", &c1.to_hex()]);
    git_cmd(dir.path(), &["notes", "add", "-m", "n2", &c2.to_hex()]);

    let repo = open_grit(dir.path());
    let entries = read_notes_tree(&repo, "refs/notes/commits").expect("read");
    assert_eq!(entries.len(), 2);
    let names: Vec<String> = entries
        .iter()
        .filter_map(|e| grit_lib::notes::note_object_name(&e.path))
        .collect();
    assert!(names.contains(&c1.to_hex()));
    assert!(names.contains(&c2.to_hex()));
}

#[test]
fn t3306_notes_prune_via_remove() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "z", "z\n", "z");
    git_cmd(
        dir.path(),
        &["notes", "add", "-m", "gone", &commit.to_hex()],
    );

    let repo = open_grit(dir.path());
    let mut entries = read_notes_tree(&repo, "refs/notes/commits").expect("read");
    entries.retain(|e| {
        grit_lib::notes::note_object_name(&e.path).as_deref() != Some(commit.to_hex().as_str())
    });
    if entries.is_empty() {
        git_cmd(dir.path(), &["update-ref", "-d", "refs/notes/commits"]);
    } else {
        write_notes_commit(&repo, "refs/notes/commits", &entries, "prune note").expect("write");
    }
    assert_fsck_strict(dir.path());
    let show = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["notes", "show", &commit.to_hex()])
        .output()
        .expect("git");
    assert!(!show.status.success(), "note should be removed");
}

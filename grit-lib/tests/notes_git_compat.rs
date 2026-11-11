//! Notes tree read/write/merge round-trips with system `git`.

use grit_lib::error::NotesError;
use std::collections::HashMap;
use std::io::Write;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::notes::{upsert_note_entry, NotesMergeStrategy, NotesRef};
use grit_lib::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::NotesWriteOptions;
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
    let entries = repo.notes().read_tree(notes_ref).ok()?;
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
    let mut entries = repo
        .notes()
        .read_tree("refs/notes/commits")
        .unwrap_or_default();
    upsert_note_entry(&mut entries, &object.to_hex(), blob);
    repo.notes()
        .write_tree_commit("refs/notes/commits", &entries, message)
        .expect("write notes");
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
    let entries = repo
        .notes()
        .read_tree("refs/notes/commits")
        .expect("read git fanout");
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
        grit_repo
            .notes()
            .merge_refs("refs/notes/commits", "refs/notes/remote", grit_strategy)
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
    grit_repo
        .notes()
        .merge_refs(
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
    let entries = repo.notes().read_tree("refs/notes/commits").expect("read");
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
    let mut entries = repo.notes().read_tree("refs/notes/commits").expect("read");
    entries.retain(|e| {
        grit_lib::notes::note_object_name(&e.path).as_deref() != Some(commit.to_hex().as_str())
    });
    if entries.is_empty() {
        git_cmd(dir.path(), &["update-ref", "-d", "refs/notes/commits"]);
    } else {
        repo.notes()
            .write_tree_commit("refs/notes/commits", &entries, "prune note")
            .expect("write");
    }
    assert_fsck_strict(dir.path());
    let show = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["notes", "show", &commit.to_hex()])
        .output()
        .expect("git");
    assert!(!show.status.success(), "note should be removed");
}

fn notes_write_options(message: &str) -> NotesWriteOptions {
    NotesWriteOptions {
        message: message.into(),
        committer: "Test <t@example.com> 1700000000 +0000"
            .parse()
            .expect("signature"),
        author: None,
    }
}

fn fanout_two_hex_prefix(object: &ObjectId) -> [u8; 2] {
    let hex = object.to_hex();
    let bytes = hex.as_bytes();
    [bytes[0], bytes[1]]
}

fn tree_name_eq_two_hex(name: &[u8], prefix: &[u8; 2]) -> bool {
    name.len() == 2
        && name[0].eq_ignore_ascii_case(&prefix[0])
        && name[1].eq_ignore_ascii_case(&prefix[1])
}

fn count_leaves_in_fanout_bucket(
    repo: &Repository,
    notes_ref: &str,
    prefix: &[u8; 2],
) -> Option<usize> {
    let root = notes_root_tree_oids(repo, notes_ref);
    let (_, subtree) = root
        .iter()
        .find(|(name, _)| tree_name_eq_two_hex(name, prefix))?;
    let tree_obj = repo.odb().read(subtree).ok()?;
    Some(parse_tree(&tree_obj.data).ok()?.len())
}

fn root_has_fanout_bucket(repo: &Repository, notes_ref: &str, prefix: &[u8; 2]) -> bool {
    notes_root_tree_oids(repo, notes_ref)
        .iter()
        .any(|(name, _)| tree_name_eq_two_hex(name, prefix))
}

fn count_full_hex_note_blobs_at_root(repo: &Repository, notes_ref: &str) -> usize {
    notes_root_tree_oids(repo, notes_ref)
        .iter()
        .filter(|(name, _)| name.len() == 40)
        .count()
}

fn notes_root_uses_fanout(repo: &Repository, notes_ref: &str) -> bool {
    notes_root_tree_oids(repo, notes_ref)
        .iter()
        .any(|(name, _)| name.len() == 2)
}

/// Pick an annotated object whose 2-hex fanout bucket holds exactly one note.
fn find_noted_object_in_singleton_bucket(
    repo: &Repository,
    notes_ref: &str,
) -> (ObjectId, [u8; 2]) {
    let entries = repo.notes().read_tree(notes_ref).expect("read notes");
    let mut by_prefix: HashMap<[u8; 2], Vec<ObjectId>> = HashMap::new();
    for entry in entries {
        let Some(hex) = grit_lib::notes::note_object_name(&entry.path) else {
            continue;
        };
        let object: ObjectId = hex.parse().expect("note object id");
        by_prefix
            .entry(fanout_two_hex_prefix(&object))
            .or_default()
            .push(object);
    }
    for (prefix, objects) in by_prefix {
        if objects.len() == 1 {
            return (objects[0], prefix);
        }
    }
    panic!("fixture must include at least one fanout bucket holding exactly one note");
}

fn notes_root_tree_oids(repo: &Repository, notes_ref: &str) -> Vec<(Vec<u8>, ObjectId)> {
    let tip = repo
        .references()
        .resolve(grit_lib::refs::store::RefNameRef::new(&notes_ref).expect("valid ref name"))
        .expect("notes tip");
    let tree_oid = match repo.odb().read(&tip).expect("read tip").kind {
        ObjectKind::Commit => {
            parse_commit(&repo.odb().read(&tip).unwrap().data)
                .expect("parse commit")
                .tree
        }
        ObjectKind::Tree => tip,
        other => panic!("unexpected notes tip kind {other:?}"),
    };
    parse_tree(&repo.odb().read(&tree_oid).expect("read tree").data)
        .expect("parse tree")
        .into_iter()
        .map(|e| (e.name, e.oid))
        .collect()
}

fn git_write_stdin(dir: &std::path::Path, args: &[&str], input: &[u8]) -> String {
    let mut child = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn uppercase_hex_leaf_read_and_add_sees_existing_note() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let commit = seed_commit(dir.path(), "f", "body\n", "init");
    let note_blob = git_write_stdin(
        dir.path(),
        &["hash-object", "-w", "--stdin"],
        b"note from git\n",
    );
    let upper_name = commit.to_hex().to_ascii_uppercase();
    let tree_line = format!("100644 blob {note_blob}\t{upper_name}\n");
    let tree_oid = git_write_stdin(dir.path(), &["mktree"], tree_line.as_bytes());
    let notes_commit = git_cmd(
        dir.path(),
        &["commit-tree", &tree_oid, "-m", "notes with uppercase leaf"],
    )
    .trim()
    .to_owned();
    git_cmd(
        dir.path(),
        &["update-ref", "refs/notes/commits", &notes_commit],
    );

    assert_eq!(
        git_note_show(dir.path(), &commit).trim(),
        "note from git",
        "system git must read uppercase-named note leaf"
    );

    let repo = open_grit(dir.path());
    assert_eq!(
        repo.notes()
            .read(&NotesRef::commits(), &commit)
            .expect("read")
            .as_deref(),
        Some(b"note from git\n" as &[u8]),
        "grit read must find uppercase hex leaf"
    );

    let outcome = repo
        .notes()
        .add(
            &NotesRef::commits(),
            &commit,
            b"ignored\n",
            &notes_write_options("would duplicate"),
            false,
        )
        .expect_err("note already exists");
    assert!(
        matches!(
            outcome,
            grit_lib::error::Error::Notes(NotesError::AlreadyExists { .. })
        ),
        "add without force must not treat uppercase leaf as missing"
    );

    let entries = repo.notes().read_tree("refs/notes/commits").expect("list");
    let for_object = entries
        .iter()
        .filter(|e| {
            grit_lib::notes::note_object_name(&e.path).as_deref() == Some(commit.to_hex().as_str())
        })
        .count();
    assert_eq!(
        for_object, 1,
        "must not duplicate note entries for one object"
    );
    assert_fsck_strict(dir.path());
}

#[test]
fn fanout_add_force_splice_updates_one_subtree() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("seed"), "s\n").expect("write");
    git_cmd(dir.path(), &["add", "seed"]);
    git_cmd(dir.path(), &["commit", "-qm", "seed"]);

    for i in 0..300 {
        std::fs::write(dir.path().join("f"), format!("v{i}\n")).expect("write");
        git_cmd(dir.path(), &["add", "f"]);
        git_cmd(dir.path(), &["commit", "-qm", &format!("c{i}")]);
        git_cmd(
            dir.path(),
            &["notes", "add", "-m", &format!("note-{i}"), "HEAD"],
        );
    }
    assert_fsck_strict(dir.path());

    let head: ObjectId = git_cmd(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("HEAD");
    let repo = open_grit(dir.path());
    let before = notes_root_tree_oids(&repo, "refs/notes/commits");
    repo.notes()
        .add(
            &NotesRef::commits(),
            &head,
            b"replaced\n",
            &notes_write_options("replace"),
            true,
        )
        .expect("force replace");
    let after = notes_root_tree_oids(&repo, "refs/notes/commits");
    assert_eq!(
        before.len(),
        after.len(),
        "root fanout width must stay the same"
    );
    let changed = before
        .iter()
        .zip(after.iter())
        .filter(|((n0, o0), (n1, o1))| n0 == n1 && o0 != o1)
        .count();
    assert_eq!(
        changed, 1,
        "exactly one root subtree oid should change on replace"
    );
    assert_fsck_strict(dir.path());
    assert_eq!(
        git_note_show(dir.path(), &head).trim(),
        "replaced",
        "git must read grit splice replace"
    );
}

#[test]
fn flat_tree_256th_insert_via_add_refans_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let repo = open_grit(dir.path());

    let mut object_ids = Vec::with_capacity(256);
    for i in 0..256 {
        object_ids.push(seed_commit(
            dir.path(),
            &format!("f{i}"),
            &format!("{i}\n"),
            &format!("c{i}"),
        ));
    }

    let mut entries = Vec::new();
    for (i, object) in object_ids.iter().take(255).enumerate() {
        let blob = repo
            .odb()
            .write(ObjectKind::Blob, format!("note-{i}\n").as_bytes())
            .expect("blob");
        upsert_note_entry(&mut entries, &object.to_hex(), blob);
    }
    repo.notes()
        .write_tree_commit("refs/notes/commits", &entries, "255 flat notes")
        .expect("write");
    assert_fsck_strict(dir.path());

    assert_eq!(
        count_full_hex_note_blobs_at_root(&repo, "refs/notes/commits"),
        255,
        "255-note tree must stay flat at the root"
    );
    assert!(
        !notes_root_uses_fanout(&repo, "refs/notes/commits"),
        "255 notes must not fan out yet"
    );

    let head = object_ids[255];
    repo.notes()
        .add(
            &NotesRef::commits(),
            &head,
            b"256th note\n",
            &notes_write_options("256th"),
            false,
        )
        .expect("256th add");

    assert_fsck_strict(dir.path());
    assert!(
        notes_root_uses_fanout(&repo, "refs/notes/commits"),
        "256th note must fan out the notes root"
    );
    assert!(
        count_full_hex_note_blobs_at_root(&repo, "refs/notes/commits") <= 0xff,
        "notes root must not hold more than 255 flat blob entries after fanout"
    );
    assert_eq!(
        grit_note_blob(&repo, "refs/notes/commits", &head).as_deref(),
        Some("256th note\n")
    );
}

#[test]
fn flat_tree_insert_missing_note_via_add() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let c1 = seed_commit(dir.path(), "a", "1\n", "one");
    let c2 = seed_commit(dir.path(), "b", "2\n", "two");
    git_cmd(dir.path(), &["notes", "add", "-m", "n1", &c1.to_hex()]);

    let repo = open_grit(dir.path());
    repo.notes()
        .add(
            &NotesRef::commits(),
            &c2,
            b"grit note\n",
            &notes_write_options("add"),
            false,
        )
        .expect("insert");
    assert_fsck_strict(dir.path());
    assert_eq!(
        grit_note_blob(&repo, "refs/notes/commits", &c2).as_deref(),
        Some("grit note\n")
    );
}

#[test]
fn fanout_remove_last_note_in_bucket() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("seed"), "s\n").expect("write");
    git_cmd(dir.path(), &["add", "seed"]);
    git_cmd(dir.path(), &["commit", "-qm", "seed"]);

    for i in 0..300 {
        std::fs::write(dir.path().join("f"), format!("v{i}\n")).expect("write");
        git_cmd(dir.path(), &["add", "f"]);
        git_cmd(dir.path(), &["commit", "-qm", &format!("c{i}")]);
        git_cmd(
            dir.path(),
            &["notes", "add", "-m", &format!("note-{i}"), "HEAD"],
        );
    }

    let repo = open_grit(dir.path());
    let (target, prefix) = find_noted_object_in_singleton_bucket(&repo, "refs/notes/commits");
    assert_eq!(
        count_leaves_in_fanout_bucket(&repo, "refs/notes/commits", &prefix),
        Some(1),
        "chosen bucket must contain exactly one note leaf before remove"
    );
    assert!(root_has_fanout_bucket(&repo, "refs/notes/commits", &prefix));

    repo.notes()
        .remove(&NotesRef::commits(), &target, &notes_write_options("rm"))
        .expect("remove");

    assert!(
        !root_has_fanout_bucket(&repo, "refs/notes/commits", &prefix),
        "empty fanout bucket must be removed from the notes root tree"
    );
    assert_fsck_strict(dir.path());
    let show = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["notes", "show", &target.to_hex()])
        .output()
        .expect("git");
    assert!(
        !show.status.success(),
        "git notes show must fail after removing the note"
    );
}

//! Hard reset on a wide work tree: directory-relative stat scans, rewriting files whose size
//! changed, and keeping the stat data of files it just checked out. Checked against the
//! system `git` on the same repositories.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};
use tempfile::TempDir;

/// More tracked files than the threshold at which stat scans switch to directory handles.
const WIDE_FILES: usize = 1200;
const FILES_PER_DIR: usize = 40;

fn ident() -> grit_lib::ident::Signature {
    "T <t@e.c> 1700000000 +0000".parse().expect("signature")
}

fn hard_reset_to_head(dir: &Path) {
    let repo =
        Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repository");
    let head = grit_test_support::rev_parse(dir, "HEAD");
    reset_to_commit(
        &repo,
        &ResetOptions::new(head, ResetMode::Hard, ident(), "reset: moving to HEAD"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("hard reset");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@e.c"]);
}

fn rel(i: usize) -> String {
    format!("d{:02}/f{i:04}.txt", i / FILES_PER_DIR)
}

/// A committed repository with [`WIDE_FILES`] files spread over several directories.
fn wide_repo() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);
    for i in 0..WIDE_FILES {
        let path = dir.join(rel(i));
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, format!("file {i}\n")).expect("write");
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "wide"]);
    tmp
}

/// Edits of every kind a hard reset must undo: grown, same-size, deleted and touched files.
fn dirty_wide_repo(dir: &Path) {
    for i in (0..WIDE_FILES).step_by(97) {
        fs::write(dir.join(rel(i)), format!("file {i} grown by an edit\n")).expect("grow");
    }
    for i in (5..WIDE_FILES).step_by(101) {
        let original = format!("file {i}\n");
        fs::write(dir.join(rel(i)), "x".repeat(original.len() - 1) + "\n").expect("same size");
    }
    for i in (11..WIDE_FILES).step_by(89) {
        fs::remove_file(dir.join(rel(i))).expect("delete");
    }
    for i in (13..WIDE_FILES).step_by(83) {
        let path = dir.join(rel(i));
        let content = fs::read(&path).expect("read");
        fs::write(&path, content).expect("touch");
    }
}

fn git_status(dir: &Path) -> String {
    git_cmd(&["status", "--porcelain", "--untracked-files=no"])
        .in_dir(dir)
        .suc()
        .stdout
}

fn git_index_oids(dir: &Path) -> String {
    git_cmd(&["ls-files", "-s"]).in_dir(dir).suc().stdout
}

#[test]
fn wide_hard_reset_restores_every_edit_like_git() {
    let ours = wide_repo();
    let theirs = TempDir::new().expect("tempdir");
    git(
        theirs.path(),
        &["clone", "-q", &ours.path().display().to_string(), "."],
    );
    dirty_wide_repo(ours.path());
    dirty_wide_repo(theirs.path());

    hard_reset_to_head(ours.path());
    git(theirs.path(), &["reset", "-q", "--hard", "HEAD"]);

    assert_eq!(git_status(ours.path()), "", "git sees edits left behind");
    assert_eq!(git_index_oids(ours.path()), git_index_oids(theirs.path()));
    for i in 0..WIDE_FILES {
        assert_eq!(
            fs::read_to_string(ours.path().join(rel(i))).expect("read"),
            format!("file {i}\n"),
            "{}",
            rel(i)
        );
    }
    git(ours.path(), &["fsck", "--no-progress"]);
}

/// `* text eol=lf`: a CRLF copy of a committed LF file cleans to the same blob, but its size
/// changed, so hard reset rewrites the checked-out (LF) form, as Git does.
#[test]
fn hard_reset_rewrites_a_resized_file_that_cleans_to_the_same_blob() {
    let ours = wide_repo();
    let dir = ours.path();
    fs::write(dir.join(".gitattributes"), "*.txt text eol=lf\n").expect("attributes");
    git(dir, &["add", ".gitattributes"]);
    git(dir, &["commit", "-qm", "attributes"]);
    let theirs = TempDir::new().expect("tempdir");
    git(
        theirs.path(),
        &["clone", "-q", &dir.display().to_string(), "."],
    );
    for root in [dir, theirs.path()] {
        fs::write(root.join(rel(7)), "file 7\r\n").expect("crlf");
    }

    hard_reset_to_head(dir);
    git(theirs.path(), &["reset", "-q", "--hard", "HEAD"]);

    assert_eq!(
        fs::read(dir.join(rel(7))).expect("ours"),
        fs::read(theirs.path().join(rel(7))).expect("theirs")
    );
    assert_eq!(fs::read(dir.join(rel(7))).expect("ours"), b"file 7\n");
    assert_eq!(git_status(dir), "");
}

/// A smudge filter that the clean filter does not undo: the checked-out bytes do not hash to
/// the blob. Git keeps the stat data of a file it just wrote; grit must not re-read the fresh
/// file as racy and smudge its recorded size.
#[test]
fn hard_reset_keeps_stat_of_files_it_checked_out() {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);
    git(dir, &["config", "filter.up.smudge", "tr a-z A-Z"]);
    git(dir, &["config", "filter.up.clean", "cat"]);
    fs::write(dir.join(".gitattributes"), "*.up filter=up\n").expect("attributes");
    fs::write(dir.join("a.up"), "hello\n").expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "filtered"]);
    let blob = ObjectId::from_hex(git(dir, &["rev-parse", "HEAD:a.up"]).trim()).expect("oid");
    fs::write(dir.join("a.up"), "an unrelated edit\n").expect("dirty");

    hard_reset_to_head(dir);

    assert_eq!(fs::read(dir.join("a.up")).expect("read"), b"HELLO\n");
    let staged = git(dir, &["ls-files", "-s", "a.up"]);
    assert!(staged.contains(&blob.to_hex()), "{staged}");
    let debug = git(dir, &["ls-files", "--debug", "a.up"]);
    assert!(
        debug.contains("size: 6"),
        "fresh checkout must keep its size: {debug}"
    );
}

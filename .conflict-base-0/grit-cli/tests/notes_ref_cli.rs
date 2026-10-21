//! Regression: `--ref` must work after the notes subcommand (documented synopsis).

use std::process::Command;

use grit_test_support::git;
use grit_test_support::CliFixture;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn seed_commit(fixture: &CliFixture) {
    git(fixture.path(), &["commit", "--allow-empty", "-m", "seed"]);
}

#[test]
fn notes_list_ref_after_subcommand() {
    let fixture = CliFixture::empty();
    seed_commit(&fixture);
    let out = fixture.run(&["notes", "list", "--ref", "refs/notes/custom"]);
    assert!(out.ok(), "{}", out.dump("notes list --ref"));
    assert!(
        out.stdout.contains("No notes yet") || out.stdout.contains("\"action\""),
        "unexpected stdout: {}",
        out.stdout
    );
}

#[test]
fn notes_show_ref_after_subcommand() {
    let fixture = CliFixture::empty();
    seed_commit(&fixture);
    fixture.run_ok(&[
        "notes",
        "add",
        "HEAD",
        "-m",
        "hi",
        "--ref",
        "refs/notes/custom",
    ]);
    let out = fixture.run(&["notes", "show", "HEAD", "--ref", "refs/notes/custom"]);
    assert!(out.ok(), "{}", out.dump("notes show --ref"));
    assert!(out.stdout.contains("hi"), "stdout: {}", out.stdout);
}

#[test]
fn notes_add_ref_after_subcommand() {
    let fixture = CliFixture::empty();
    seed_commit(&fixture);
    let out = fixture.run(&[
        "notes",
        "add",
        "HEAD",
        "-m",
        "placed ref",
        "--ref",
        "refs/notes/custom",
    ]);
    assert!(out.ok(), "{}", out.dump("notes add --ref"));
}

#[test]
fn padded_git_notes_ref_rejects_add() {
    let fixture = CliFixture::empty();
    seed_commit(&fixture);
    let mut cmd = Command::new(GRIT);
    cmd.current_dir(fixture.path())
        .args(["notes", "add", "HEAD", "-m", "must fail"])
        .env("GIT_NOTES_REF", " refs/notes/spaced ");
    grit_test_support::configure_git_env(&mut cmd);
    cmd.env("TZ", "UTC").env("NO_COLOR", "1");
    let out = cmd.output().expect("spawn grit");
    assert_ne!(
        out.status.code(),
        Some(0),
        "grit must reject padded GIT_NOTES_REF (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !fixture.path().join(".git/refs/notes/spaced").exists(),
        "must not create a trimmed notes ref"
    );
}

#[test]
fn empty_git_notes_ref_rejects_add() {
    let fixture = CliFixture::empty();
    seed_commit(&fixture);
    fixture.run_ok(&["config", "user.name", "Test"]);
    fixture.run_ok(&["config", "user.email", "t@example.com"]);
    let mut cmd = Command::new(GRIT);
    cmd.current_dir(fixture.path())
        .args(["notes", "add", "HEAD", "-m", "must fail"])
        .env("GIT_NOTES_REF", "");
    grit_test_support::configure_git_env(&mut cmd);
    cmd.env("TZ", "UTC").env("NO_COLOR", "1");
    let out = cmd.output().expect("spawn grit");
    assert_ne!(
        out.status.code(),
        Some(0),
        "grit must reject empty GIT_NOTES_REF (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    let notes_ref = fixture.path().join(".git/refs/notes/commits");
    assert!(
        !notes_ref.exists(),
        "must not fall back to refs/notes/commits"
    );
}

#[test]
fn notes_remove_ref_after_subcommand() {
    let fixture = CliFixture::empty();
    seed_commit(&fixture);
    fixture.run_ok(&["notes", "add", "HEAD", "-m", "drop me"]);
    let out = fixture.run(&["notes", "remove", "HEAD", "--ref", "refs/notes/commits"]);
    assert!(out.ok(), "{}", out.dump("notes remove --ref"));
}

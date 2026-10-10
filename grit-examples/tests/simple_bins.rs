use std::fs;
use std::process::Command;

use anyhow::{bail, Context, Result};
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::init_repository;

fn run_bin(bin: &str, args: &[&str], cwd: &std::path::Path) -> Result<String> {
    let exe = match bin {
        "gritx-add" => env!("CARGO_BIN_EXE_gritx-add"),
        "gritx-cat-file" => env!("CARGO_BIN_EXE_gritx-cat-file"),
        "gritx-commit-tree" => env!("CARGO_BIN_EXE_gritx-commit-tree"),
        "gritx-for-each-ref" => env!("CARGO_BIN_EXE_gritx-for-each-ref"),
        "gritx-log" => env!("CARGO_BIN_EXE_gritx-log"),
        "gritx-ls-files" => env!("CARGO_BIN_EXE_gritx-ls-files"),
        "gritx-merge" => env!("CARGO_BIN_EXE_gritx-merge"),
        "gritx-read-tree" => env!("CARGO_BIN_EXE_gritx-read-tree"),
        "gritx-status" => env!("CARGO_BIN_EXE_gritx-status"),
        "gritx-update-ref" => env!("CARGO_BIN_EXE_gritx-update-ref"),
        "gritx-write-tree" => env!("CARGO_BIN_EXE_gritx-write-tree"),
        other => bail!("unknown test binary {other}"),
    };
    let output = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run {bin}"))?;
    if !output.status.success() {
        bail!("{bin} failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn simple_bins_cover_a_tiny_commit_workflow() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = init_repository(
        temp.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )?;
    fs::write(temp.path().join("hello.txt"), "hello\n")?;

    run_bin("gritx-add", &["hello.txt"], temp.path())?;
    assert_eq!(run_bin("gritx-ls-files", &[], temp.path())?, "hello.txt\n");

    let tree = run_bin("gritx-write-tree", &[], temp.path())?
        .trim()
        .to_owned();
    let commit = run_bin(
        "gritx-commit-tree",
        &[&tree, "-m", "initial example"],
        temp.path(),
    )?
    .trim()
    .to_owned();
    run_bin(
        "gritx-update-ref",
        &["refs/heads/main", &commit],
        temp.path(),
    )?;

    let listed_refs = run_bin("gritx-for-each-ref", &["refs/heads"], temp.path())?;
    assert!(listed_refs.contains("refs/heads/main"));
    assert!(listed_refs.contains(&commit));

    let log = run_bin("gritx-log", &[], temp.path())?;
    assert!(log.contains("commit "));
    assert!(log.contains("initial example"));

    let raw_commit = run_bin("gritx-cat-file", &[&commit], temp.path())?;
    assert!(raw_commit.contains("tree "));
    assert!(raw_commit.contains("initial example"));

    let object = repo.odb.read(&commit.parse()?)?;
    assert_eq!(object.kind, ObjectKind::Commit);
    assert_eq!(parse_commit(&object.data)?.message, "initial example\n");
    assert_eq!(
        refs::resolve_ref(&repo.git_dir, "refs/heads/main")?.to_string(),
        commit
    );

    fs::remove_file(temp.path().join("hello.txt"))?;
    run_bin("gritx-read-tree", &["-u", &commit], temp.path())?;
    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt"))?,
        "hello\n"
    );

    fs::write(temp.path().join("hello.txt"), "hello again\n")?;
    run_bin("gritx-add", &["hello.txt"], temp.path())?;
    let second_tree = run_bin("gritx-write-tree", &[], temp.path())?
        .trim()
        .to_owned();
    let second_commit = run_bin(
        "gritx-commit-tree",
        &[&second_tree, "-p", &commit, "-m", "second example"],
        temp.path(),
    )?
    .trim()
    .to_owned();
    run_bin("gritx-merge", &[&second_commit], temp.path())?;
    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt"))?,
        "hello again\n"
    );
    assert_eq!(
        refs::resolve_ref(&repo.git_dir, "refs/heads/main")?.to_string(),
        second_commit
    );

    let status = run_bin("gritx-status", &[], temp.path())?;
    assert!(status.is_empty(), "expected clean status, got {status:?}");
    Ok(())
}

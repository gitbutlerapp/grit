//! `GIT_OBJECT_DIRECTORY` selects where `grit` reads and writes objects (issue #1907).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn command(program: &str, dir: &Path, objects: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Odb Env")
        .env("GIT_AUTHOR_EMAIL", "odb-env@example.com")
        .env("GIT_COMMITTER_NAME", "Odb Env")
        .env("GIT_COMMITTER_EMAIL", "odb-env@example.com")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_OBJECT_DIRECTORY", objects);
    cmd
}

fn ok(mut cmd: Command) -> String {
    let out: Output = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn loose_count(objects: &Path) -> usize {
    fs::read_dir(objects)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().len() == 2 && e.path().is_dir())
                .map(|e| fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(unix)]
#[test]
fn cat_add_and_commit_use_env_object_directory() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("r");
    let objq = tmp.path().join("objq");
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&objq).unwrap();
    let mut init = Command::new("git");
    init.current_dir(&work).args(["init", "-q", "-b", "main"]);
    ok(init);
    let dot_git_objects = work.join(".git/objects");

    fs::write(work.join("fb"), "fresh blob\n").unwrap();
    let hex = ok(command("git", &work, &objq, &["hash-object", "-w", "fb"]));
    assert_eq!(
        ok(command(grit, &work, &objq, &["cat", &hex])),
        "fresh blob"
    );

    fs::write(work.join("nb"), "new\n").unwrap();
    ok(command(grit, &work, &objq, &["add", "nb"]));
    assert_eq!(loose_count(&objq), 2);
    assert_eq!(loose_count(&dot_git_objects), 0);

    ok(command(grit, &work, &objq, &["commit", "-m", "x"]));
    assert_eq!(
        loose_count(&dot_git_objects),
        0,
        "nothing leaks into .git/objects"
    );
    ok(command(
        "git",
        &work,
        &objq,
        &["fsck", "--strict", "--no-dangling"],
    ));
    assert_eq!(
        ok(command("git", &work, &objq, &["cat-file", "-p", "HEAD:nb"])),
        "new"
    );
}

//! `commit` JSON `kind` for active work-tree case collisions (issue #2581).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{entry_from_stat, Index, IndexWriteOptions, MODE_REGULAR};
use grit_lib::objects::ObjectKind;
use grit_lib::porcelain::commit::{CommitRequest, CommitScope};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit(dir: &Path, args: &[&str]) -> (i32, String) {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let out = Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_SYSTEM", null)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("grit");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn enable_ignorecase(repo: &Repository, root: &Path) -> grit_lib::error::Result<()> {
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tignorecase = true\n[user]\n\tname = Test\n\temail = t@example.com\n",
    )
    .map_err(grit_lib::error::Error::from)?;
    repo.reload_config()
}

fn build_readme_collision_repo(root: &Path) -> grit_lib::error::Result<()> {
    let dest = root.to_path_buf();
    let (repo, _) = Repository::init(
        &dest,
        &InitOptions::new()
            .with_initial_branch("master".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )?;
    enable_ignorecase(&repo, &dest)?;

    let oid_upper = repo.odb().write(ObjectKind::Blob, b"UPPER\n")?;
    let oid_lower = repo.odb().write(ObjectKind::Blob, b"lower\n")?;
    fs::write(dest.join("readme.txt"), b"lower\n")?;
    let mut index = Index::new();
    index.add_or_replace(entry_from_stat(
        &dest.join("readme.txt"),
        b"Readme.TXT",
        oid_upper,
        MODE_REGULAR,
    )?);
    index.add_or_replace(entry_from_stat(
        &dest.join("readme.txt"),
        b"readme.txt",
        oid_lower,
        MODE_REGULAR,
    )?);
    index.sort();
    write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())?;
    repo.index()
        .write(&mut index, &IndexWriteOptions::default())?;
    let mut req = CommitRequest::new("collide");
    req.message = "collide".into();
    repo.commits().create(
        &grit_lib::commits::CommitOptions::new(req, CommitScope::Staged),
        &mut NullProgress,
        &CancelToken::never(),
    )?;
    Ok(())
}

#[test]
fn commit_json_worktree_case_collision_kind() -> TestResult {
    let tmp = tempfile::tempdir()?;
    build_readme_collision_repo(tmp.path())?;

    let (code, stdout) = grit(tmp.path(), &["commit", "-m", "next", "--json"]);
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(v["kind"].as_str(), Some("worktree_case_collision"));
    assert!(
        v["error"]
            .as_str()
            .is_some_and(|e| e.contains("collide on this filesystem")),
        "{v}"
    );
    Ok(())
}

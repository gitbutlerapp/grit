//! Validate objects with fsck rules and run a repository hook.
//!
//! Source for the library guide "Maintenance" page (included in the docs site).

use grit_lib::command_runner::hooks::{run_hook, HookResult};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::fsck::{fsck_object, FsckObjectOptions};
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use std::fs;
use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    let mut opts = RepositoryOptions::from_process();
    if let Some(wt) = work_tree {
        opts = opts.with_work_tree(wt);
    }
    Repository::open(&git_dir, &opts)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let repo = open_repo(&root)?;

    let head = resolve_head(repo.git_dir())?;
    let commit = head
        .oid()
        .cloned()
        .ok_or_else(|| grit_lib::error::Error::Message("HEAD has no commit".into()))?;

    let obj = repo.objects().read(&commit)?;
    fsck_object(
        ObjectKind::Commit,
        &obj.data,
        FsckObjectOptions::new(HashAlgo::Sha1),
    )
    .map_err(|e| grit_lib::error::Error::Message(e.report_line()))?;
    println!("fsck_object_ok=1");

    let hook_path = repo.git_dir().join("hooks").join("pre-commit");
    if let Some(parent) = hook_path.parent() {
        fs::create_dir_all(parent).map_err(grit_lib::error::Error::Io)?;
    }
    fs::write(&hook_path, b"#!/bin/sh\nexit 0\n").map_err(grit_lib::error::Error::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))
            .map_err(grit_lib::error::Error::Io)?;
    }

    let hook_result = run_hook(&repo, "pre-commit", &[], None)?;
    let ran = matches!(hook_result, HookResult::Success { .. });
    println!("pre_commit_ran={ran}");

    Ok(())
}

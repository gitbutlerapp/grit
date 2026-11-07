//! Validate objects with fsck rules and run a repository hook.
//!
//! Source for the library guide "Maintenance" page (included in the docs site).

use anyhow::Result;
use grit_lib::command_runner::hooks::{run_hook, HookResult};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::fsck::{fsck_object, FsckObjectOptions};
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::Repository;

use std::fs;
use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository> {
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
    Ok(Repository::open(&git_dir, &opts)?)
}

fn main() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| anyhow::anyhow!("missing repository path"))?;
    let repo = open_repo(&root)?;

    let head = repo.references().head()?;
    let commit = head
        .oid()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no commit"))?;

    let obj = repo.objects().read(&commit)?;
    fsck_object(
        ObjectKind::Commit,
        &obj.data,
        FsckObjectOptions::new(HashAlgo::Sha1),
    )
    .map_err(|e| anyhow::anyhow!("fsck_object failed: {e:?}"))?;
    println!("fsck_object_ok=1");

    let hook_path = repo.git_dir().join("hooks").join("pre-commit");
    if let Some(parent) = hook_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&hook_path, b"#!/bin/sh\nexit 0\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))?;
    }

    let hook_result = run_hook(&repo, "pre-commit", &[], None)?;
    let ran = matches!(hook_result, HookResult::Success { .. });
    println!("pre_commit_ran={ran}");

    Ok(())
}

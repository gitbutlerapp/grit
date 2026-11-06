//! Create a bundle file and verify ref tips with [`RepositoryBundles`](grit_lib::repo_handles::RepositoryBundles).
//!
//! Source for the library guide "Bundle" page (included in the docs site).

use anyhow::Result;
use grit_lib::bundle::BundleSpec;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
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
    let bundle_path = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("guide.bundle"));

    let repo = open_repo(&root)?;
    let head = resolve_head(repo.git_dir())?;
    let tip: ObjectId = head
        .oid()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no commit"))?;

    repo.bundles().create(
        &BundleSpec {
            include: vec![(tip, "refs/heads/main".into())],
            ..Default::default()
        },
        &bundle_path,
    )?;

    let report = repo.bundles().verify(&bundle_path)?;
    println!("bundle_ok={}", report.is_ok());

    let heads = repo.bundles().list_heads(&bundle_path, None)?.items;
    for head in heads {
        println!("bundle_head {} {}", head.name, head.oid.to_hex());
    }

    Ok(())
}

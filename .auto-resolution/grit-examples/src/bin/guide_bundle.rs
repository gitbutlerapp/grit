//! Create a bundle file and verify ref tips with [`RepositoryBundles`](grit_lib::repo_handles::RepositoryBundles).
//!
//! Source for the library guide "Bundle" page (included in the docs site).

use grit_lib::bundle::BundleSpec;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
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
    let mut opts = RepositoryOptions::empty();
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
    let bundle_path = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("guide.bundle"));

    let repo = open_repo(&root)?;
    let head = resolve_head(repo.git_dir())?;
    let tip: ObjectId = head
        .oid()
        .cloned()
        .ok_or_else(|| grit_lib::error::Error::Message("HEAD has no commit".into()))?;

    repo.bundles()
        .create(
            &BundleSpec {
                include: vec![(tip, "refs/heads/main".into())],
                ..Default::default()
            },
            &bundle_path,
        )
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;

    let report = repo
        .bundles()
        .verify(&bundle_path)
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;
    println!("bundle_ok={}", report.is_ok());

    let heads = repo
        .bundles()
        .list_heads(&bundle_path)
        .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?;
    for head in heads {
        println!("bundle_head {} {}", head.name, head.oid.to_hex());
    }

    Ok(())
}

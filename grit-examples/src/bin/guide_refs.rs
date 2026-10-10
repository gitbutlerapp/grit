//! Resolve HEAD, list branches and tags, update a branch ref with reflog.
//!
//! Source for the library guide "Refs" page (included in the docs site).

use grit_lib::objects::ObjectId;
use grit_lib::reflog::read_reflog;
use grit_lib::refs::{self, Ref};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use std::path::{Path, PathBuf};

const DEMO_BRANCH: &str = "refs/heads/library-guide-demo";

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
    Repository::open(&git_dir, work_tree)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let repo = open_repo(&root)?;
    let git_dir = &repo.git_dir;

    let head = resolve_head(git_dir)?;
    match &head {
        grit_lib::state::HeadState::Branch { refname, oid, .. } => {
            println!("head_symbolic={refname}");
            if let Some(oid) = oid {
                println!("head_oid={}", oid.to_hex());
            }
        }
        grit_lib::state::HeadState::Detached { oid } => {
            println!("head_detached={}", oid.to_hex());
        }
        grit_lib::state::HeadState::Invalid => println!("head_invalid=1"),
    }

    let head_file = refs::read_ref_file(&git_dir.join("HEAD"))?;
    if let Ref::Symbolic(target) = head_file {
        println!("head_file_symbolic={target}");
    }

    for (name, oid) in refs::list_refs(git_dir, "refs/heads/")? {
        println!("branch {name} {}", oid.to_hex());
    }
    for (name, oid) in refs::list_refs(git_dir, "refs/tags/")? {
        println!("tag {name} {}", oid.to_hex());
    }

    println!("ref_storage={}", repo.refs().format());

    let target = head
        .oid()
        .cloned()
        .ok_or_else(|| grit_lib::error::Error::Message("HEAD has no commit".into()))?;

    let old = refs::resolve_ref(git_dir, DEMO_BRANCH).ok();
    refs::write_ref(git_dir, DEMO_BRANCH, &target)?;

    let zero = ObjectId::zero();
    let old_oid = old.as_ref().unwrap_or(&zero);
    let identity = "Library Guide <guide@grit-scm.com> 1735689600 +0000";
    refs::append_reflog(
        git_dir,
        DEMO_BRANCH,
        old_oid,
        &target,
        identity,
        "library guide refs example",
        true,
    )?;

    println!("demo_ref={DEMO_BRANCH}");
    println!("demo_oid={}", target.to_hex());

    let entries = read_reflog(git_dir, DEMO_BRANCH)?;
    if let Some(last) = entries.last() {
        println!("demo_reflog_new={}", last.new_oid.to_hex());
        println!("demo_reflog_message={}", last.message);
    }

    Ok(())
}

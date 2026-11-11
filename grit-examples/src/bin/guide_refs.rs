//! Resolve HEAD, list branches and tags, update a branch ref with reflog.
//!
//! Source for the library guide "Refs" page (included in the docs site).

use anyhow::Result;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::listing::{RefNamespace, RefQuery, RefSort, SortKey};
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::store::RefName;
use grit_lib::refs::{self, Ref};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::ReflogCreate;

use std::path::{Path, PathBuf};

const DEMO_BRANCH: &str = "refs/heads/library-guide-demo";

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
    let git_dir = repo.git_dir();

    let head = repo.references().head()?;
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

    for branch in repo.branches().list_simple()?.branches {
        let marker = if branch.current { "*" } else { " " };
        println!("branch {marker} {} {}", branch.name, branch.oid.to_hex());
    }
    let tag_query = RefQuery::default()
        .with_namespaces(vec![RefNamespace::Tags])
        .with_sort(RefSort::default().with_key(SortKey::Name));
    let tag_rows = repo
        .tags()
        .list(&grit_lib::repo_handles::TagListOptions::full_annotations(
            tag_query,
        ))?
        .into_summaries()
        .ok_or_else(|| anyhow::anyhow!("expected annotated tag summaries"))?;
    for tag in tag_rows {
        println!("tag {} {} {:?}", tag.name, tag.oid.to_hex(), tag.kind);
    }

    println!("ref_storage={}", repo.ref_store().format());

    let target = head
        .oid()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("HEAD has no commit"))?;

    let demo = RefName::try_from(DEMO_BRANCH)
        .map_err(|err| anyhow::anyhow!("invalid demo ref name: {err}"))?;
    let old = repo.references().resolve(&demo).ok();
    repo.references().update(&demo, &target)?;

    let zero = ObjectId::zero();
    let old_oid = old.as_ref().unwrap_or(&zero);
    let identity = "Library Guide <guide@grit-scm.com> 1735689600 +0000";
    repo.reflogs().append(
        &demo,
        &ReflogEntry {
            old_oid: *old_oid,
            new_oid: target,
            identity: identity.to_owned(),
            message: "library guide refs example".to_owned(),
        },
        ReflogCreate::Always,
    )?;

    println!("demo_ref={DEMO_BRANCH}");
    println!("demo_oid={}", target.to_hex());

    let entries = repo.references().reflog(&demo)?;
    if let Some(last) = entries.last() {
        println!("demo_reflog_new={}", last.new_oid.to_hex());
        println!("demo_reflog_message={}", last.message);
    }

    Ok(())
}

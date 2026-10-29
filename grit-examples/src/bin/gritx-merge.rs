use anyhow::{bail, Result};
use clap::Parser;
use grit_examples::{checkout_tree, commit_tree, head_oid, resolve_name};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::objects::{parse_commit, ObjectKind};
use grit_lib::refs::read_symbolic_ref_for_repository;
use grit_lib::refs::store::RefName;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-merge",
    version,
    about = "Do a tiny fast-forward-only merge"
)]
struct Cli {
    commit: String,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo = Repository::discover(None, &RepositoryOptions::from_process())?;
    let target = resolve_name(&repo, &cli.commit)?;
    let head = head_oid(&repo)?;
    if !is_ancestor(&repo, head, target)? {
        bail!("simple example merge only supports fast-forwards");
    }

    let target_tree = commit_tree(&repo, target)?;
    let mut index = Index::new();
    for entry in grit_examples::entries_from_tree(&repo, target_tree)? {
        index.add_or_replace(entry);
    }
    repo.index_handle().write(&mut index)?;
    checkout_tree(&repo, target_tree)?;

    if let Some(target_ref) = read_symbolic_ref_for_repository(&repo, "HEAD")? {
        let name = RefName::try_from(target_ref.as_str())
            .map_err(|e| anyhow::anyhow!("invalid HEAD target: {e}"))?;
        repo.references().update(&name, &target)?;
    } else {
        let head = RefName::try_from("HEAD")
            .map_err(|e| anyhow::anyhow!("invalid HEAD ref name: {e}"))?;
        repo.references().update(&head, &target)?;
    }
    println!("Fast-forwarded to {target}");
    Ok(())
}

fn is_ancestor(
    repo: &Repository,
    ancestor: grit_lib::objects::ObjectId,
    commit: grit_lib::objects::ObjectId,
) -> Result<bool> {
    let mut stack = vec![commit];
    let mut seen = std::collections::HashSet::new();
    while let Some(oid) = stack.pop() {
        if oid == ancestor {
            return Ok(true);
        }
        if !seen.insert(oid) {
            continue;
        }
        let object = repo.odb().read(&oid)?;
        if object.kind != ObjectKind::Commit {
            continue;
        }
        stack.extend(parse_commit(&object.data)?.parents);
    }
    Ok(false)
}

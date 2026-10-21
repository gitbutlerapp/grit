use anyhow::Result;
use clap::Parser;
use grit_examples::{checkout_tree, commit_tree, entries_from_tree, resolve_name};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-read-tree",
    version,
    about = "Replace the index with a tree"
)]
struct Cli {
    /// Also write files to the work tree.
    #[arg(short = 'u', long)]
    update_worktree: bool,
    treeish: String,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let repo = Repository::discover(None, &RepositoryOptions::empty())?;
    let oid = resolve_name(&repo, &cli.treeish)?;
    let tree_oid = match repo.odb().read(&oid)?.kind {
        ObjectKind::Commit => commit_tree(&repo, oid)?,
        ObjectKind::Tree => oid,
        _ => anyhow::bail!("{oid} is not a tree or commit"),
    };
    let mut index = Index::new();
    for entry in entries_from_tree(&repo, tree_oid)? {
        index.add_or_replace(entry);
    }
    repo.index_handle().write(&mut index)?;
    if cli.update_worktree {
        checkout_tree(&repo, tree_oid)?;
    }
    Ok(())
}

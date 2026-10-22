use anyhow::Result;
use clap::Parser;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::repo::Repository;

#[derive(Debug, Parser)]
#[command(
    name = "gritx-write-tree",
    version,
    about = "Write the current index as a tree"
)]
struct Cli;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let _cli = Cli::parse();
    let repo = Repository::discover(None, &RepositoryOptions::empty())?;
    let index = repo.index_handle().load()?;
    let oid = write_tree_from_index(repo.odb(), &index, "")?;
    println!("{oid}");
    Ok(())
}
